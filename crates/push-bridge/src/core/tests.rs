//! The core's behaviour against a [`TestBackend`] and a [`TestMessenger`], in paused
//! time. The end-to-end flows are in `tests/bridge.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use push_bridge_control::{
    AgentInfo, BackendInfo, BridgeState, Event as ControlEvent, EventLine, PauseParams,
};
use push_sdk::{ActivityState, ClientMsgId, ConnectionState, Event, MessageKind, SdkError, UserId};
use serde_json::json;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;

use super::*;
use crate::backend::{
    Answer, BackendStatus, ErrorKind, NotRestored, PendingMessages, Prepared, RequestKind,
    Restored, Started, SteerOutcome, TaskStatus, TurnSource,
};
use crate::cards;
use crate::control::{Action, ControlHub, Reply};
use crate::folders;
use crate::messenger::{Call, TestMessenger, param, template};
use crate::test_backend::{TestBackend, error};
use crate::test_dir::TestDir;

type Channels = (mpsc::UnboundedReceiver<BackendEvent>, mpsc::Receiver<Event>);

pub(super) struct Harness {
    pub(super) messenger: Arc<TestMessenger>,
    pub(super) backend: Arc<TestBackend>,
    state: Arc<StateFile>,
    root: PathBuf,
    pub(super) work: PathBuf,
    other: PathBuf,
    channels: Option<Channels>,
    stop: Option<oneshot::Sender<()>>,
    run: Option<JoinHandle<Result<(), BridgeError>>>,
    hub: ControlHub,
    events: broadcast::Receiver<EventLine>,
    /// Calls are taken with this (see `voice_tests`).
    pub(super) voice: Option<crate::voice::VoiceSetup>,
    /// Set while a call is in progress (see `voice_tests`).
    pub(super) call_flag: Option<CallFlag>,
    _dir: TestDir,
}

impl Harness {
    fn new() -> Self {
        Self::named("Test")
    }

    /// With a backend named `name`.
    pub(super) fn named(name: &str) -> Self {
        let dir = TestDir::new();
        dir.mkdir("root/work");
        dir.mkdir("root/other");
        let root = dir.path().join("root");
        let real = |folder: &str| {
            folders::resolve(folder, &root, std::slice::from_ref(&root)).expect("a test folder")
        };
        let (work, other, root) = (real("work"), real("other"), real("."));
        let state = Arc::new(StateFile::load(&dir.path().join("state.json")).unwrap());
        let (messenger, messenger_events) = TestMessenger::new();
        let (sender, backend_events) = mpsc::unbounded_channel();
        let backend = Arc::new(TestBackend::new(name, sender));
        let hub = ControlHub::new("0011223344556677".into(), true);
        let events = hub.handle().subscribe();
        Self {
            messenger,
            backend,
            state,
            root,
            work,
            other,
            channels: Some((backend_events, messenger_events)),
            stop: None,
            run: None,
            hub,
            events,
            voice: None,
            call_flag: None,
            _dir: dir,
        }
    }

    pub(super) fn start(&mut self) {
        let (backend_events, messenger_events) = self.channels.take().expect("not started");
        let mut bridge = Bridge::new(
            Arc::clone(&self.messenger),
            Arc::clone(&self.backend) as Arc<dyn Backend>,
            backend_events,
            messenger_events,
            Arc::clone(&self.state),
            BridgeOptions {
                roots: vec![self.root.clone()],
                default_folder: self.work.clone(),
                default_mode: PermissionMode::Default,
            },
        )
        .with_control(self.hub.handle(), self.hub.calls());
        if let Some(voice) = self.voice.take() {
            bridge = bridge.with_voice(voice);
        }
        if let Some(flag) = self.call_flag.take() {
            bridge = bridge.with_call_flag(flag);
        }
        let (stop, stopped) = oneshot::channel::<()>();
        self.stop = Some(stop);
        self.run = Some(tokio::spawn(bridge.run(async {
            let _ = stopped.await;
        })));
    }

    async fn started() -> Self {
        let mut harness = Self::new();
        harness.start();
        harness.backend.wait_for("start", 1).await;
        settle().await;
        harness
    }

    pub(super) async fn stop(&mut self) -> Result<(), BridgeError> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.run.take().expect("running").await.unwrap()
    }

    pub(super) fn replies(&self) -> Vec<String> {
        self.messenger.reply_texts()
    }

    /// The cards sent (not replies): `(id, template)`.
    fn cards(&self) -> Vec<(ClientMsgId, String)> {
        self.messenger
            .sent_rich()
            .into_iter()
            .map(|(id, body)| (id, template(&body)))
            .filter(|(_, tpl)| !tpl.starts_with("agent.reply@"))
            .collect()
    }

    /// The `count`th card, once sent.
    async fn card(&self, count: usize) -> ClientMsgId {
        self.messenger
            .wait_until("a card", |_| self.cards().len() >= count)
            .await;
        self.cards()[count - 1].0
    }

    pub(super) async fn reply_containing(&self, text: &str) {
        self.messenger
            .wait_until(text, |_| {
                self.replies().iter().any(|reply| reply.contains(text))
            })
            .await;
    }

    pub(super) async fn idle(&self) {
        self.messenger
            .wait_until("idle", |_| {
                self.messenger.activities().last() == Some(&(ActivityState::Idle, None))
            })
            .await;
    }

    pub(super) fn count(&self, prefix: &str) -> usize {
        self.backend
            .calls()
            .iter()
            .filter(|call| call.starts_with(prefix))
            .count()
    }

    fn start_call(&self, folder: &Path, session: &str) -> String {
        format!(
            "start folder={} session={session} mode=default",
            folder.display()
        )
    }

    /// Asks the bridge to do `action` over the control channel.
    pub(super) async fn call(&self, action: Action) -> Reply {
        self.hub.call(action).await
    }

    /// The control events emitted since the last look.
    pub(super) fn control_events(&mut self) -> Vec<ControlEvent> {
        let mut events = Vec::new();
        while let Ok(line) = self.events.try_recv() {
            events.push(line.event);
        }
        events
    }

    /// The states in `events`, in order.
    fn states(events: &[ControlEvent]) -> Vec<BridgeState> {
        events
            .iter()
            .filter_map(|event| match event {
                ControlEvent::StateChanged { state, .. } => Some(*state),
                _ => None,
            })
            .collect()
    }

    /// Writes the backend's program file with `content` and reports it as its executable.
    fn program(&self, content: &str) -> PathBuf {
        let path = self._dir.path().join("coda.exe");
        std::fs::write(&path, content).unwrap();
        self.backend.set_executable(Some(path.clone()));
        path
    }

    /// Asks for a pause (reason "upgrade") over the control channel; the reply comes
    /// once the backend is released.
    fn pause(&self, drain_timeout_ms: Option<u64>) -> JoinHandle<Reply> {
        let hub = self.hub.clone();
        tokio::spawn(async move {
            hub.call(Action::Pause(PauseParams {
                reason: Some("upgrade".into()),
                drain_timeout_ms,
            }))
            .await
        })
    }

    /// The control channel's status snapshot.
    pub(super) fn control_status(&self) -> push_bridge_control::Status {
        self.hub.handle().status()
    }

    /// The pause's phase on the control channel.
    fn pause_phase(&self) -> Option<String> {
        self.hub.handle().status().pause.map(|pause| pause.phase)
    }

    /// The `start` calls, in order.
    fn starts(&self) -> Vec<String> {
        self.backend
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("start"))
            .collect()
    }
}

/// Lets the core handle everything waiting: paused time moves on only when every task
/// is idle.
pub(super) async fn settle() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

pub(super) fn permission() -> RequestKind {
    RequestKind::Permission {
        tool: "Bash".into(),
        preview: "cargo test".into(),
    }
}

fn exited() -> BackendEvent {
    BackendEvent::Exited {
        reason: "gone".into(),
    }
}

#[tokio::test(start_paused = true)]
async fn the_start_resumes_the_stored_session_of_the_stored_folder() {
    let mut h = Harness::new();
    let other = h.other.clone();
    h.state
        .update(|state| {
            state.folder = Some(other.clone());
            state.sessions.insert(folder_key(&other), "s-9".into());
        })
        .unwrap();
    h.start();
    h.backend.wait_for("start", 1).await;
    assert_eq!(h.backend.calls()[0], h.start_call(&h.other, "s-9"));
}

#[tokio::test(start_paused = true)]
async fn an_unknown_stored_session_starts_a_new_one_and_stores_it() {
    let mut h = Harness::new();
    let work = h.work.clone();
    h.state
        .update(|state| {
            state.sessions.insert(folder_key(&work), "gone".into());
        })
        .unwrap();
    h.backend.fail_next_start(ErrorKind::SessionNotFound);
    h.start();
    h.backend.wait_for("start", 2).await;
    settle().await;
    assert_eq!(
        h.backend.calls()[..2],
        [h.start_call(&h.work, "gone"), h.start_call(&h.work, "None")]
    );
    let state = h.state.get();
    assert_eq!(state.sessions[&folder_key(&h.work)], "session-1");
    assert_eq!(state.folder.as_ref(), Some(&h.work));
}

#[tokio::test(start_paused = true)]
async fn a_stored_folder_outside_the_roots_falls_back_to_the_default() {
    let mut h = Harness::new();
    h.state
        .update(|state| state.folder = Some(std::env::temp_dir()))
        .unwrap();
    h.start();
    h.backend.wait_for("start", 1).await;
    assert_eq!(h.backend.calls()[0], h.start_call(&h.work, "None"));
}

#[tokio::test(start_paused = true)]
async fn the_same_engine_gives_the_missed_background_messages() {
    let mut h = Harness::new();
    h.state
        .update(|state| {
            state.engine_instance = Some("engine-1".into());
            state.agent_message_cursor = Some(4);
        })
        .unwrap();
    h.backend.set_pending(PendingMessages {
        messages: vec![
            (4, "Old".into(), "seen".into()),
            (5, "Build".into(), "done".into()),
        ],
        lost: true,
    });
    h.start();
    h.reply_containing("lost").await;
    assert!(
        h.backend
            .calls()
            .contains(&"pending after=4 engine=engine-1".to_owned())
    );
    let bodies = h.messenger.sent_rich();
    assert_eq!(param(&bodies[0].1, "title").as_deref(), Some("Build"));
    assert_eq!(h.replies(), ["done", "Some background messages were lost."]);
    assert_eq!(h.state.get().agent_message_cursor, Some(5));
}

#[tokio::test(start_paused = true)]
async fn another_engine_starts_the_cursor_over_without_fetching() {
    let mut h = Harness::new();
    h.state
        .update(|state| {
            state.engine_instance = Some("engine-0".into());
            state.agent_message_cursor = Some(4);
        })
        .unwrap();
    h.start();
    h.backend.wait_for("start", 1).await;
    settle().await;
    assert_eq!(h.count("pending"), 0);
    let state = h.state.get();
    assert_eq!(state.engine_instance.as_deref(), Some("engine-1"));
    assert_eq!(state.agent_message_cursor, None);
    h.backend.emit(BackendEvent::AgentMessage {
        cursor: 1,
        label: "Note".into(),
        text: "hello".into(),
        urgent: false,
    });
    h.reply_containing("hello").await;
    assert_eq!(h.state.get().agent_message_cursor, Some(1));
}

#[tokio::test(start_paused = true)]
async fn own_and_repeated_messages_are_handled_once() {
    let h = Harness::started().await;
    let own = h.messenger.message(
        h.messenger.conversation(),
        h.messenger.me(),
        MessageKind::Text,
        "echo",
    );
    h.messenger.inject(Event::MessageAdded(own));
    let message = h.messenger.owner_says("hello");
    h.messenger.inject(Event::MessageAdded(message));
    settle().await;
    let prompts: Vec<_> = h
        .backend
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("prompt"))
        .collect();
    assert_eq!(prompts, ["prompt hello"]);
}

#[tokio::test(start_paused = true)]
async fn a_stranger_s_card_answer_is_refused_silently() {
    let h = Harness::started().await;
    let answer = h.backend.request(permission());
    let card = h.card(1).await;
    h.messenger.answer_as(UserId::new(), card, &["approve"]);
    settle().await;
    assert!(h.messenger.texts().is_empty());
    h.messenger.answer(card, &["reject"]);
    assert_eq!(answer.await, Ok(Answer::Permission(false)));
}

#[tokio::test(start_paused = true)]
async fn the_owner_s_other_messages_never_reach_the_backend() {
    let h = Harness::started().await;
    let rich = r#"{"tpl":"agent.reply@1","params":{"text":"x"},"fallback":"x"}"#;
    let card = h.messenger.message(
        h.messenger.conversation(),
        h.messenger.owner_id(),
        MessageKind::Rich,
        rich,
    );
    h.messenger.inject(Event::MessageAdded(card));
    let elsewhere = h.messenger.message(
        push_sdk::ConversationId::new(),
        h.messenger.owner_id(),
        MessageKind::Text,
        "hi",
    );
    h.messenger.inject(Event::MessageAdded(elsewhere));
    settle().await;
    assert_eq!(h.count("prompt"), 0);
    assert!(h.messenger.texts().is_empty());
}

#[tokio::test(start_paused = true)]
async fn typed_text_answers_the_oldest_card() {
    let h = Harness::started().await;
    let first = h.backend.request(RequestKind::Question {
        question: "Which?".into(),
        options: vec!["Red".into(), "Blue".into()],
        multi_select: false,
        allow_free_text: false,
    });
    let second = h.backend.request(permission());
    h.card(2).await;
    h.messenger.owner_says("green");
    h.reply_containing("Pick one of the options").await;
    h.messenger.owner_says("2");
    assert_eq!(first.await, Ok(Answer::Question("Blue".into())));
    h.messenger.owner_says("y");
    assert_eq!(second.await, Ok(Answer::Permission(true)));
    assert_eq!(h.count("prompt"), 0);
}

#[tokio::test(start_paused = true)]
async fn card_answers_map_to_labels_in_option_order_and_plan_approvals() {
    let h = Harness::started().await;
    let question = h.backend.request(RequestKind::Question {
        question: "Which?".into(),
        options: vec!["A".into(), "B".into(), "C".into()],
        multi_select: true,
        allow_free_text: false,
    });
    let card = h.card(1).await;
    assert_eq!(h.cards()[0].1, "agent.choose@1");
    h.messenger.answer(card, &["o3", "o1"]);
    assert_eq!(question.await, Ok(Answer::Question("A, C".into())));
    let plan = h.backend.request(RequestKind::Plan {
        plan: "1. edit".into(),
    });
    let card = h.card(2).await;
    assert_eq!(h.cards()[1].1, "agent.plan-approval@1");
    h.messenger.answer(card, &["approve"]);
    assert_eq!(plan.await, Ok(Answer::Plan(true)));
}

#[tokio::test(start_paused = true)]
async fn a_question_with_many_options_is_text_answered_by_text() {
    let h = Harness::started().await;
    let options: Vec<String> = (1..=11).map(|i| format!("Option {i}")).collect();
    let answer = h.backend.request(RequestKind::Question {
        question: "Pick".into(),
        options,
        multi_select: false,
        allow_free_text: false,
    });
    h.reply_containing("Option 11").await;
    assert!(h.cards().is_empty());
    assert_eq!(
        h.messenger.activities().last(),
        Some(&(ActivityState::Waiting, Some("question".into())))
    );
    h.messenger.owner_says("option 11");
    assert_eq!(answer.await, Ok(Answer::Question("Option 11".into())));
}

#[tokio::test(start_paused = true)]
async fn a_refused_card_declines_its_request_and_says_why() {
    let h = Harness::started().await;
    h.messenger
        .fail_next_rich(SdkError::Invalid("template refused".into()));
    let answer = h.backend.request(permission());
    assert_eq!(answer.await, Ok(Answer::Decline));
    h.reply_containing("Could not show the card (invalid); the request was declined.")
        .await;
    h.idle().await;
    h.messenger.owner_says("hello");
    h.backend.wait_for("prompt hello", 1).await;
}

#[tokio::test(start_paused = true)]
async fn without_steer_text_waits_for_the_turn() {
    let h = Harness::started().await;
    h.backend.set_capabilities([Capability::Sessions]);
    h.backend.turn_started(TurnSource::Engine);
    h.messenger.owner_says("one");
    h.messenger.owner_says("two");
    settle().await;
    assert_eq!(h.count("steer") + h.count("prompt"), 0);
    h.backend.turn_ended(false);
    h.backend.wait_for("prompt", 1).await;
    assert!(h.backend.calls().contains(&"prompt one\n\ntwo".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn commands_a_backend_lacks_are_not_supported() {
    let h = Harness::started().await;
    h.backend.set_capabilities([Capability::Steer]);
    for command in ["/fork", "/btw hi", "/schedule list", "/mode plan", "/tasks"] {
        h.messenger.owner_says(command);
    }
    settle().await;
    assert_eq!(h.replies(), vec!["Not supported by Test."; 5]);
    assert_eq!(h.count("command"), 0);
}

#[tokio::test(start_paused = true)]
async fn other_prompt_errors_are_replied_with_method_and_code() {
    let h = Harness::started().await;
    h.backend
        .next_prompt(Err(error("session/prompt", ErrorKind::Other)));
    h.messenger.owner_says("go");
    h.reply_containing("session/prompt failed (-32000): failed")
        .await;
}

#[tokio::test(start_paused = true)]
async fn busy_then_no_active_turn_prompts_again_after_a_pause() {
    let h = Harness::started().await;
    h.backend
        .next_prompt(Err(error("session/prompt", ErrorKind::Busy)));
    h.backend.next_steer(Ok(SteerOutcome::NoActiveTurn));
    h.messenger.owner_says("go");
    h.backend.wait_for("prompt go", 2).await;
    let calls = h.backend.timed_calls();
    let calls = &calls[1..];
    let names: Vec<_> = calls.iter().map(|(_, call)| call.as_str()).collect();
    assert_eq!(names, ["prompt go", "steer go", "prompt go"]);
    assert!(calls[2].0 - calls[1].0 >= Duration::from_millis(500));
}

/// Coda reports the turn's end before it frees its slot: a prompt right after it meets
/// a busy agent whose turn is ending, with no turn running here to wait for.
#[tokio::test(start_paused = true)]
async fn a_turn_ending_without_a_turn_here_prompts_again_after_a_pause() {
    let h = Harness::started().await;
    h.backend
        .next_prompt(Err(error("session/prompt", ErrorKind::Busy)));
    h.backend.next_steer(Ok(SteerOutcome::TurnEnding));
    h.messenger.owner_says("go");
    h.backend.wait_for("prompt go", 2).await;
    let calls = h.backend.timed_calls();
    assert!(calls[3].0 - calls[2].0 >= Duration::from_millis(500));
}

#[tokio::test(start_paused = true)]
async fn the_text_queued_at_a_turn_s_end_is_not_lost_when_the_agent_is_still_busy() {
    let h = Harness::started().await;
    h.backend.set_capabilities([Capability::Sessions]);
    h.backend.turn_started(TurnSource::Prompt);
    h.messenger.owner_says("next");
    settle().await;
    h.backend
        .next_prompt(Err(error("session/prompt", ErrorKind::Busy)));
    h.backend.turn_ended(false);
    h.backend.wait_for("prompt next", 2).await;
}

#[tokio::test(start_paused = true)]
async fn an_agent_that_stays_busy_gets_bounded_retries_then_the_owner_hears_it() {
    let h = Harness::started().await;
    for _ in 0..20 {
        h.backend
            .next_prompt(Err(error("session/prompt", ErrorKind::Busy)));
        h.backend.next_steer(Ok(SteerOutcome::TurnEnding));
    }
    let start = tokio::time::Instant::now();
    h.messenger.owner_says("go");
    h.reply_containing("not delivered").await;
    let prompts: Vec<_> = h
        .backend
        .timed_calls()
        .into_iter()
        .filter(|(_, call)| call == "prompt go")
        .map(|(at, _)| at)
        .collect();
    assert!((3..=10).contains(&prompts.len()), "{}", prompts.len());
    for pair in prompts.windows(2) {
        assert!(pair[1] - pair[0] >= Duration::from_millis(500));
    }
    let took = start.elapsed();
    assert!(
        took >= Duration::from_secs(15) && took <= Duration::from_secs(35),
        "{took:?}"
    );
    assert!(h.replies().last().unwrap().contains("go"));
}

#[tokio::test(start_paused = true)]
async fn texts_during_a_compact_or_fork_wait_for_it() {
    for command in ["/compact", "/fork"] {
        let h = Harness::started().await;
        let release = h.backend.hold_next_command();
        h.messenger.owner_says(command);
        h.backend.wait_for("command", 1).await;
        h.messenger.owner_says("one");
        h.messenger.owner_says("two");
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(h.count("prompt") + h.count("steer"), 0, "{command}");
        drop(release);
        h.backend.wait_for("prompt one\n\ntwo", 1).await;
    }
}

#[tokio::test(start_paused = true)]
async fn replies_are_edited_at_most_every_two_seconds() {
    let h = Harness::started().await;
    h.backend.turn_started(TurnSource::Prompt);
    h.backend.text("a");
    h.messenger
        .wait_until("the first reply", |_| h.messenger.sent_rich().len() == 1)
        .await;
    let first = tokio::time::Instant::now();
    h.backend.text("b");
    h.messenger
        .wait_until("an edit", |_| !h.messenger.edits().is_empty())
        .await;
    assert!(
        first.elapsed() >= Duration::from_millis(1_500),
        "{:?}",
        first.elapsed()
    );
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(h.messenger.edits().len(), 1, "no change, no edit");
    h.backend.turn_ended(false);
    h.idle().await;
    let (_, body) = h.messenger.edits().pop().unwrap();
    assert_eq!(param(&body, "text").as_deref(), Some("ab"));
    assert_eq!(h.messenger.sent_rich().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn working_activity_names_the_tool_and_is_refreshed_every_15_seconds() {
    let h = Harness::started().await;
    h.backend.turn_started(TurnSource::Prompt);
    h.backend.emit(BackendEvent::ToolStarted {
        id: "1".into(),
        tool: "Bash".into(),
        target: "cargo test".into(),
        task: None,
    });
    tokio::time::sleep(Duration::from_secs(46)).await;
    let working: Vec<_> = h
        .messenger
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Activity { state, label, .. } => Some((state, label)),
            _ => None,
        })
        .collect();
    assert!(working.len() >= 4, "{working:?}");
    assert_eq!(
        working.last(),
        Some(&(ActivityState::Working, Some("Bash cargo test".into())))
    );
}

#[tokio::test(start_paused = true)]
async fn background_tasks_and_errors() {
    let h = Harness::started().await;
    h.backend.emit(BackendEvent::TaskCompleted {
        id: "t".into(),
        description: "build".into(),
        status: TaskStatus::Failed,
        background: true,
        report: Some("\nerror: password=hunter2 rejected\nmore".into()),
    });
    h.reply_containing("Task build failed.").await;
    let reply = h.replies().pop().unwrap();
    assert!(reply.contains("error: password=«masked»"), "{reply}");
    assert!(!reply.contains("more"), "{reply}");
    h.backend.emit(BackendEvent::TaskCompleted {
        id: "f".into(),
        description: "fg".into(),
        status: TaskStatus::Completed,
        background: false,
        report: None,
    });
    h.backend.emit(BackendEvent::Error("rate limited".into()));
    h.reply_containing("rate limited").await;
    assert_eq!(h.replies().len(), 2, "foreground tasks are not shown");

    h.backend.turn_started(TurnSource::Prompt);
    h.backend.emit(BackendEvent::TaskCompleted {
        id: "t2".into(),
        description: "lint".into(),
        status: TaskStatus::Completed,
        background: true,
        report: None,
    });
    h.backend.emit(BackendEvent::Error("overloaded".into()));
    h.backend.turn_ended(false);
    h.idle().await;
    let (_, body) = h.messenger.edits().pop().unwrap();
    assert!(body.contains("Task lint completed"), "{body}");
    assert!(param(&body, "text").unwrap().contains("overloaded"));
}

#[tokio::test(start_paused = true)]
async fn a_reconnect_sends_activity_again_and_fetches_messages() {
    let h = Harness::started().await;
    h.backend.turn_started(TurnSource::Prompt);
    h.messenger
        .inject(Event::ConnectionStateChanged(ConnectionState::Connected));
    settle().await;
    assert_eq!(h.count("pending"), 0, "not a reconnect");
    let before = h.messenger.calls().len();
    h.messenger.inject(Event::ConnectionStateChanged(
        ConnectionState::Reconnecting {
            attempt: 1,
            retry_in_ms: 1_000,
        },
    ));
    h.messenger
        .inject(Event::ConnectionStateChanged(ConnectionState::Connected));
    h.backend.wait_for("pending", 1).await;
    settle().await;
    let resent = h.messenger.calls()[before..].iter().any(|call| {
        matches!(
            call,
            Call::Activity {
                state: ActivityState::Working,
                ..
            }
        )
    });
    assert!(resent);
}

#[tokio::test(start_paused = true)]
async fn signing_out_and_removal_stop_the_bridge() {
    for (event, expected) in [
        (Event::SignedOut, BridgeError::SignedOut),
        (
            Event::ConnectionStateChanged(ConnectionState::SignedOut),
            BridgeError::SignedOut,
        ),
        (Event::DeviceRemoved, BridgeError::DeviceRemoved),
        (Event::OpenedElsewhere, BridgeError::OpenedElsewhere),
    ] {
        let mut h = Harness::started().await;
        h.messenger.inject(event);
        assert_eq!(h.run.take().unwrap().await.unwrap(), Err(expected));
        assert!(h.backend.calls().contains(&"stop".to_owned()));
    }
    let mut h = Harness::started().await;
    assert_eq!(h.stop().await, Ok(()));
    assert_eq!(h.backend.calls().last().map(String::as_str), Some("stop"));
}

#[tokio::test(start_paused = true)]
async fn an_interrupted_turn_closes_its_cards() {
    let h = Harness::started().await;
    h.backend.turn_started(TurnSource::Prompt);
    h.backend.text("working");
    let answer = h.backend.request(permission());
    h.card(1).await;
    h.messenger.owner_says("/stop");
    h.backend.wait_for("interrupt", 1).await;
    drop(answer);
    h.backend.turn_ended(true);
    h.reply_containing("Declined: the turn was stopped.").await;
    h.idle().await;
    let (_, body) = h.messenger.edits().pop().unwrap();
    assert_eq!(
        param(&body, "text").as_deref(),
        Some("working"),
        "the reply sealed for the card stays as it was; the decline says why"
    );
    h.messenger.owner_says("next");
    h.backend.wait_for("prompt next", 1).await;
}

#[tokio::test(start_paused = true)]
async fn status_help_usage_and_backend_commands() {
    let h = Harness::started().await;
    h.backend.set_status(BackendStatus {
        model: Some("m-1".into()),
        version: Some("9.9".into()),
        running_tasks: Some(2),
    });
    h.messenger.owner_says("/status");
    h.reply_containing("Running tasks: 2").await;
    let status = h.replies().pop().unwrap();
    for line in [
        "Session: session-1".to_owned(),
        "Folder: ".to_owned(),
        "Model: m-1".to_owned(),
        "Permission mode: default".to_owned(),
        "State: idle".to_owned(),
        "Test: 9.9".to_owned(),
    ] {
        assert!(status.contains(&line), "{status}");
    }
    assert!(
        status.contains(&format!("Folder: {}\n", h.work.display())),
        "{status}"
    );
    h.messenger.owner_says("/help");
    h.reply_containing("/schedule list").await;
    h.messenger.owner_says("/effort huge");
    h.reply_containing("Usage: /effort").await;
    h.backend.next_command(Ok(CommandReply {
        text: "m-1 (current)\nm-2".into(),
        session: None,
    }));
    h.messenger.owner_says("/model");
    h.reply_containing("m-2").await;
    h.messenger.owner_says("/model m-2");
    h.backend.wait_for("command SetModel(\"m-2\")", 1).await;
    h.backend
        .next_command(Err(error("session/compact", ErrorKind::Refused)));
    h.messenger.owner_says("/compact");
    h.reply_containing("session/compact failed (refused): refused")
        .await;
    h.messenger.owner_says("/review this");
    h.backend.wait_for("prompt /review this", 1).await;
}

#[tokio::test(start_paused = true)]
async fn idle_only_commands_wait_and_new_waits_for_the_turn() {
    let h = Harness::started().await;
    h.backend.turn_started(TurnSource::Prompt);
    for command in ["/compact", "/fork", "/resume x", "/cd ../other"] {
        h.messenger.owner_says(command);
    }
    settle().await;
    assert_eq!(
        h.replies()[1..],
        vec!["Wait until the turn ends."; 4],
        "after the turn's reply"
    );
    h.messenger.owner_says("/new");
    h.backend.wait_for("interrupt", 1).await;
    assert_eq!(h.count("start"), 1);
    h.backend.turn_ended(true);
    h.reply_containing("New session session-2").await;
}

#[tokio::test(start_paused = true)]
async fn a_new_session_waiting_for_the_turn_survives_an_exit() {
    let h = Harness::started().await;
    h.backend.turn_started(TurnSource::Prompt);
    h.messenger.owner_says("/new");
    h.backend.wait_for("interrupt", 1).await;
    h.backend.emit(exited());
    h.backend.wait_for("start", 2).await;
    let starts: Vec<_> = h
        .backend
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("start"))
        .collect();
    assert_eq!(starts[1], h.start_call(&h.work, "None"));
}

#[tokio::test(start_paused = true)]
async fn resume_and_fork_store_their_sessions() {
    let h = Harness::started().await;
    h.backend.fail_next_start(ErrorKind::SessionNotFound);
    h.messenger.owner_says("/resume nope");
    h.reply_containing("No session nope in this folder.").await;
    h.messenger.owner_says("/resume s-7");
    h.reply_containing("Resumed session s-7.").await;
    assert_eq!(h.state.get().sessions[&folder_key(&h.work)], "s-7");
    h.backend.next_command(Ok(CommandReply {
        text: "Forked.".into(),
        session: Some("s-8".into()),
    }));
    h.messenger.owner_says("/fork");
    h.reply_containing("Forked.").await;
    assert_eq!(h.state.get().sessions[&folder_key(&h.work)], "s-8");
    h.messenger.owner_says("/status");
    h.reply_containing("Session: s-8").await;
}

#[tokio::test(start_paused = true)]
async fn cd_new_and_resume_close_the_old_session_s_requests() {
    for (command, reply) in [
        ("/cd ../other", "Folder "),
        ("/new", "New session"),
        ("/resume s-7", "Resumed session s-7."),
    ] {
        let h = Harness::started().await;
        let answer = h.backend.request(permission());
        h.card(1).await;
        h.messenger.owner_says(command);
        h.reply_containing(reply).await;
        h.reply_containing("Declined: ").await;
        assert!(answer.await.is_err(), "{command}: declined");
        h.messenger.owner_says("hello");
        h.backend.wait_for("prompt hello", 1).await;
        tokio::time::sleep(Duration::from_secs(3_600)).await;
        assert!(
            !h.replies().iter().any(|r| r.contains("No answer; skipped")),
            "{command}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn bypass_approved_before_new_does_not_reach_the_new_session() {
    let h = Harness::started().await;
    h.messenger.owner_says("/mode bypass");
    h.card(1).await;
    let release = h.backend.hold_next_command();
    h.messenger.owner_says("y");
    h.backend
        .wait_for("command SetPermissionMode(bypassPermissions)", 1)
        .await;
    h.messenger.owner_says("/new");
    h.reply_containing("New session session-2").await;
    drop(release);
    h.backend
        .wait_for("command SetPermissionMode(default)", 1)
        .await;
    h.reply_containing("was dropped").await;
    assert!(
        !h.replies().iter().any(|r| r.contains("Approvals are off")),
        "{:?}",
        h.replies()
    );
    h.messenger.owner_says("/status");
    h.reply_containing("Permission mode: default").await;
    h.backend.emit(exited());
    h.backend.wait_for("start", 3).await;
    let starts: Vec<_> = h
        .backend
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("start"))
        .collect();
    assert_eq!(starts[2], h.start_call(&h.work, "session-2"));
}

#[tokio::test(start_paused = true)]
async fn cd_new_and_resume_cancel_an_open_bypass_card() {
    for (command, reply) in [
        ("/cd ../other", "Folder "),
        ("/new", "New session"),
        ("/resume s-7", "Resumed session s-7."),
    ] {
        let h = Harness::started().await;
        h.messenger.owner_says("/mode bypass");
        let card = h.card(1).await;
        h.messenger.owner_says(command);
        h.reply_containing(reply).await;
        h.reply_containing("Turning approvals off was cancelled")
            .await;
        h.messenger.answer(card, &["approve"]);
        h.messenger.owner_says("y");
        h.backend.wait_for("prompt y", 1).await;
        settle().await;
        assert_eq!(h.count("command"), 0, "{command}");
        h.messenger.owner_says("/status");
        h.reply_containing("Permission mode: default").await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_fork_that_finishes_after_a_cd_is_not_stored_in_the_new_folder() {
    let h = Harness::started().await;
    let release = h.backend.hold_next_command();
    h.backend.next_command(Ok(CommandReply {
        text: "Forked.".into(),
        session: Some("s-8".into()),
    }));
    h.messenger.owner_says("/fork");
    h.backend.wait_for("command Fork", 1).await;
    h.messenger.owner_says("/cd ../other");
    h.reply_containing("new session session-2").await;
    drop(release);
    h.reply_containing("s-8").await;
    let state = h.state.get();
    assert_eq!(state.sessions[&folder_key(&h.other)], "session-2");
    assert_eq!(state.sessions[&folder_key(&h.work)], "session-1");
    h.messenger.owner_says("/status");
    h.reply_containing("Session: session-2").await;
}

#[tokio::test(start_paused = true)]
async fn cd_resumes_each_folder_s_own_session() {
    let h = Harness::started().await;
    h.messenger.owner_says("/cd ../other");
    h.reply_containing("new session session-2").await;
    assert_eq!(h.state.get().folder.as_ref(), Some(&h.other));
    h.messenger.owner_says(&format!("/cd {}", h.work.display()));
    h.reply_containing("resumed session session-1").await;
    let calls = h.backend.calls();
    assert!(calls.contains(&h.start_call(&h.other, "None")), "{calls:?}");
    assert!(
        calls.contains(&h.start_call(&h.work, "session-1")),
        "{calls:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn the_backoff_starts_over_after_a_minute_of_running() {
    let h = Harness::started().await;
    h.backend.emit(exited());
    h.backend.wait_for("start", 2).await;
    h.backend.emit(exited());
    h.backend.wait_for("start", 3).await;
    tokio::time::sleep(Duration::from_secs(61)).await;
    h.backend.emit(exited());
    h.backend.wait_for("start", 4).await;
    let times: Vec<_> = h
        .backend
        .timed_calls()
        .into_iter()
        .filter(|(_, call)| call.starts_with("start"))
        .map(|(at, _)| at)
        .collect();
    let gap = |i: usize| (times[i] - times[i - 1]).as_secs_f64();
    assert!((1.0..1.1).contains(&gap(1)), "{}", gap(1));
    assert!((2.0..2.1).contains(&gap(2)), "{}", gap(2));
    assert!(
        (62.0..62.2).contains(&gap(3)),
        "a reset backoff waits 1 s again: {}",
        gap(3)
    );
}

#[tokio::test(start_paused = true)]
async fn a_failing_first_start_is_said_once_and_retried() {
    let mut h = Harness::new();
    h.backend.fail_next_start(ErrorKind::NotRunning);
    h.backend.fail_next_start(ErrorKind::NotRunning);
    h.start();
    h.backend.wait_for("start", 3).await;
    settle().await;
    assert_eq!(
        h.replies(),
        [
            "initialize failed (exited): not running; retrying.",
            "Test started."
        ]
    );
}

/// The agent's posts in its owner's conversation, in order: `reply N: <text> [<step>
/// <status>, …]`, `edit N: …` and `card <template>`, replies numbered from 1.
fn posts(calls: &[Call]) -> Vec<String> {
    let describe = |body: &str| {
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        let text = value["params"]["text"].as_str().unwrap_or_default();
        let steps: Vec<String> = value["params"]["steps"]
            .as_array()
            .map(|steps| {
                steps
                    .iter()
                    .map(|step| format!("{} {}", step["label"], step["status"]).replace('"', ""))
                    .collect()
            })
            .unwrap_or_default();
        if steps.is_empty() {
            text.to_owned()
        } else {
            format!("{text} [{}]", steps.join(", "))
        }
    };
    let mut replies: Vec<ClientMsgId> = Vec::new();
    let mut posts = Vec::new();
    for call in calls {
        match call {
            Call::SendRich { id, body, .. } if template(body).starts_with("agent.reply@") => {
                replies.push(*id);
                posts.push(format!("reply {}: {}", replies.len(), describe(body)));
            }
            Call::SendRich { body, .. } => posts.push(format!("card {}", template(body))),
            Call::EditRich { id, body, .. } => {
                let n = replies.iter().position(|reply| reply == id).unwrap() + 1;
                posts.push(format!("edit {n}: {}", describe(body)));
            }
            _ => {}
        }
    }
    posts
}

fn bash(id: &str, target: &str) -> BackendEvent {
    BackendEvent::ToolStarted {
        id: id.into(),
        tool: "Bash".into(),
        target: target.into(),
        task: None,
    }
}

#[tokio::test(start_paused = true)]
async fn a_card_ends_the_reply_and_what_follows_goes_into_a_new_one_below_the_answer() {
    let h = Harness::started().await;
    h.backend.turn_started(TurnSource::Prompt);
    h.backend.text("Checking the time.");
    h.backend.emit(bash("a", "date"));
    let answer = h.backend.request(permission());
    let card = h.card(1).await;
    settle().await;
    assert_eq!(
        posts(&h.messenger.calls()),
        [
            "reply 1: …",
            "edit 1: Checking the time. [Bash date pending]",
            "card agent.approval@1",
        ]
    );

    h.messenger.answer(card, &["approve"]);
    assert_eq!(answer.await, Ok(Answer::Permission(true)));
    settle().await;
    let answered = posts(&h.messenger.calls()).len();
    assert_eq!(answered, 3, "approving edits nothing");

    tokio::time::sleep(Duration::from_millis(1_500)).await;
    h.backend.emit(BackendEvent::ToolFinished {
        id: "a".into(),
        failed: false,
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    h.backend.text("done");
    h.backend.turn_ended(false);
    h.idle().await;
    let after = posts(&h.messenger.calls())[answered..].to_vec();
    assert_eq!(after[0], "reply 2: … [Bash date done]");
    assert_eq!(after.last().unwrap(), "edit 2: done [Bash date done]");
    assert!(after.iter().all(|post| !post.starts_with("edit 1")));
    let detail = h
        .messenger
        .edits()
        .last()
        .and_then(|(_, body)| {
            let value: serde_json::Value = serde_json::from_str(body).ok()?;
            value["params"]["steps"][0]["detail"]
                .as_str()
                .map(str::to_owned)
        })
        .unwrap();
    assert!(detail.ends_with(" s"), "{detail}");
}

#[tokio::test(start_paused = true)]
async fn requests_in_a_row_post_their_cards_one_after_the_other() {
    let h = Harness::started().await;
    h.backend.turn_started(TurnSource::Prompt);
    h.backend.text("Two questions.");
    let first = h.backend.request(permission());
    let second = h.backend.request(RequestKind::Plan {
        plan: "1. edit".into(),
    });
    h.card(2).await;
    settle().await;
    assert_eq!(
        posts(&h.messenger.calls()),
        [
            "reply 1: …",
            "edit 1: Two questions.",
            "card agent.approval@1",
            "card agent.plan-approval@1",
        ]
    );
    let cards = h.cards();
    h.messenger.answer(cards[0].0, &["approve"]);
    h.messenger.answer(cards[1].0, &["approve"]);
    assert_eq!(first.await, Ok(Answer::Permission(true)));
    assert_eq!(second.await, Ok(Answer::Plan(true)));
    h.backend.text("Both answered.");
    h.backend.turn_ended(false);
    h.idle().await;
    assert_eq!(
        posts(&h.messenger.calls())[4..],
        ["reply 2: Both answered."]
    );
}

#[tokio::test(start_paused = true)]
async fn a_turn_that_ends_after_a_declined_card_posts_no_empty_reply() {
    let h = Harness::started().await;
    h.backend.turn_started(TurnSource::Prompt);
    h.backend.emit(bash("a", "rm -rf build"));
    let answer = h.backend.request(permission());
    let card = h.card(1).await;
    h.messenger.answer(card, &["reject"]);
    assert_eq!(answer.await, Ok(Answer::Permission(false)));
    h.backend.turn_ended(false);
    h.idle().await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        posts(&h.messenger.calls()),
        [
            "reply 1: …",
            "edit 1: … [Bash rm -rf build pending]",
            "card agent.approval@1",
        ]
    );
    assert_eq!(
        h.messenger.activities().last(),
        Some(&(ActivityState::Idle, None))
    );
}

async fn started_as(format: crate::backend::TextFormat) -> Harness {
    let mut h = Harness::new();
    h.backend.set_text_format(format);
    h.start();
    h.backend.wait_for("start", 1).await;
    settle().await;
    h
}

/// `(template, text)` of every reply sent, in order.
fn reply_templates(h: &Harness) -> Vec<(String, String)> {
    h.messenger
        .sent_rich()
        .iter()
        .map(|(_, body)| (template(body), param(body, "text").unwrap_or_default()))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_markdown_backend_s_turns_and_messages_go_out_as_reply_v2() {
    let h = started_as(crate::backend::TextFormat::Markdown).await;
    h.backend.turn_started(TurnSource::Prompt);
    h.backend.text("It is **1:32 PM**.");
    h.backend.turn_ended(false);
    h.idle().await;
    h.backend.emit(BackendEvent::AgentMessage {
        cursor: 1,
        label: "Build".into(),
        text: "`cargo test` passed".into(),
        urgent: false,
    });
    h.reply_containing("passed").await;
    h.messenger.owner_says("/help");
    h.reply_containing("/yolo").await;
    let sent = reply_templates(&h);
    assert_eq!(sent[0].0, "agent.reply@2");
    assert_eq!(
        h.messenger
            .edits()
            .last()
            .map(|(_, body)| param(body, "text")),
        Some(Some("It is **1:32 PM**.".to_owned()))
    );
    assert_eq!(
        sent[1],
        ("agent.reply@2".to_owned(), "`cargo test` passed".to_owned())
    );
    assert_eq!(
        sent[2].0, "agent.reply@1",
        "the bridge's own words stay plain"
    );
}

#[tokio::test(start_paused = true)]
async fn a_plain_backend_s_text_goes_out_as_reply_v1() {
    let h = Harness::started().await;
    h.backend.turn_started(TurnSource::Prompt);
    h.backend.text("2 * 3 * 4 = 24");
    h.backend.turn_ended(false);
    h.idle().await;
    h.backend.emit(BackendEvent::AgentMessage {
        cursor: 1,
        label: "Note".into(),
        text: "**hi**".into(),
        urgent: false,
    });
    h.reply_containing("**hi**").await;
    assert!(
        reply_templates(&h)
            .iter()
            .all(|(tpl, _)| tpl == "agent.reply@1")
    );
}

#[tokio::test(start_paused = true)]
async fn status_reports_state_folder_session_backend_and_counts() {
    let mut h = Harness::new();
    let work = h.work.clone();
    h.state
        .update(|state| {
            state.sessions.insert(folder_key(&work), "s-9".into());
        })
        .unwrap();
    h.start();
    h.backend.wait_for("start", 1).await;
    settle().await;
    let status = h.hub.handle().status();
    assert_eq!(status.state, Some(BridgeState::Idle));
    assert_eq!(status.detail, None);
    assert_eq!(status.folder, Some(h.work.display().to_string()));
    assert_eq!(status.session.as_deref(), Some("s-9"));
    assert_eq!(
        status.backend,
        Some(BackendInfo {
            name: "Test".into(),
            version: Some("1.0".into()),
            running: true,
            running_tasks: 0,
        })
    );
    assert_eq!(
        status.agent,
        Some(AgentInfo {
            display_name: "Coda on Devbox".into(),
            handle: "coda-devbox".into(),
        })
    );
    assert_eq!((status.open_requests, status.held_texts), (0, 0));
    assert_eq!(status.instance, "0011223344556677");

    h.backend.emit(BackendEvent::TaskStarted {
        id: "t1".into(),
        description: "build".into(),
        background: true,
    });
    let _answer = h.backend.request(permission());
    h.card(1).await;
    settle().await;
    let status = h.hub.handle().status();
    assert_eq!(status.state, Some(BridgeState::Waiting));
    assert_eq!(status.detail.as_deref(), Some("approval needed"));
    assert_eq!((status.open_requests, status.held_texts), (1, 0));
    assert_eq!(status.backend.map(|backend| backend.running_tasks), Some(1));
    let events = h.control_events();
    assert!(events.contains(&ControlEvent::StateChanged {
        state: BridgeState::Waiting,
        detail: Some("approval needed".into()),
    }));
    assert!(events.contains(&ControlEvent::RequestOpened {
        kind: "permission".into(),
        open: 1,
    }));
}

#[tokio::test(start_paused = true)]
async fn restart_backend_keeps_the_messenger_session() {
    let mut h = Harness::started().await;
    h.control_events();
    assert_eq!(
        h.call(Action::RestartBackend).await,
        Ok(json!({"restarted": true}))
    );
    assert_eq!(h.count("stop"), 1);
    assert_eq!(h.count("start"), 2);
    assert_eq!(h.backend.calls()[2], h.start_call(&h.work, "session-1"));
    h.reply_containing("Test restarted by the owner.").await;
    assert_eq!(
        h.replies(),
        [
            "(Test is restarting: the owner asked)",
            "Test restarted by the owner. Undelivered background messages and in-memory \
             schedules were lost.",
        ]
    );
    assert!(!h.messenger.calls().contains(&Call::Reconnect));
    let conversation = h.messenger.conversation();
    assert!(h.messenger.calls().iter().all(|call| match call {
        Call::SendRich {
            conversation: c, ..
        } => *c == conversation,
        _ => true,
    }));
    settle().await;
    let events = h.control_events();
    assert_eq!(
        Harness::states(&events),
        [BridgeState::Restarting, BridgeState::Idle]
    );
    h.messenger.owner_says("still here");
    h.backend.wait_for("prompt still here", 1).await;
}

#[tokio::test(start_paused = true)]
async fn reconnect_signs_in_again_and_keeps_the_backend() {
    let mut h = Harness::started().await;
    h.backend.turn_started(TurnSource::Prompt);
    settle().await;
    h.control_events();
    assert_eq!(
        h.call(Action::Reconnect).await,
        Ok(json!({"reconnected": true}))
    );
    let reconnects = h
        .messenger
        .calls()
        .iter()
        .filter(|call| **call == Call::Reconnect)
        .count();
    assert_eq!(reconnects, 1);
    assert_eq!(h.count("start"), 1);
    assert_eq!(h.count("stop"), 0);
    settle().await;
    assert_eq!(
        h.hub.handle().status().state,
        Some(BridgeState::Reconnecting)
    );
    let before = h.messenger.calls().len();
    h.messenger
        .inject(Event::ConnectionStateChanged(ConnectionState::Connected));
    h.backend.wait_for("pending", 1).await;
    settle().await;
    let resent = h.messenger.calls()[before..].iter().any(|call| {
        matches!(
            call,
            Call::Activity {
                state: ActivityState::Working,
                ..
            }
        )
    });
    assert!(resent, "the activity is sent again");
    let status = h.hub.handle().status();
    assert_eq!(status.state, Some(BridgeState::Working));
    assert_eq!(status.connection.as_deref(), Some("connected"));
    assert_eq!(
        Harness::states(&h.control_events()),
        [BridgeState::Reconnecting, BridgeState::Working]
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_declines_open_requests_and_stops_ok() {
    let mut h = Harness::started().await;
    let answer = h.backend.request(permission());
    h.card(1).await;
    settle().await;
    h.control_events();
    assert_eq!(
        h.call(Action::Shutdown).await,
        Ok(json!({"stopping": true}))
    );
    assert_eq!(h.run.take().unwrap().await.unwrap(), Ok(()));
    assert!(answer.await.is_err(), "declined");
    assert_eq!(
        h.replies().last().map(String::as_str),
        Some("Declined: the bridge was stopped.")
    );
    assert_eq!(h.backend.calls().last().map(String::as_str), Some("stop"));
    let events = h.control_events();
    assert!(events.contains(&ControlEvent::RequestClosed {
        kind: "permission".into(),
        outcome: "declined".into(),
        open: 0,
    }));
    assert_eq!(Harness::states(&events), [BridgeState::Stopping]);
    assert_eq!(h.hub.handle().status().state, Some(BridgeState::Stopping));
}

#[tokio::test(start_paused = true)]
async fn status_shows_the_backend_stopped_after_the_bridge_stops() {
    let mut h = Harness::started().await;
    h.backend.emit(BackendEvent::TaskStarted {
        id: "t1".into(),
        description: "build".into(),
        background: true,
    });
    settle().await;
    let running = h.hub.handle().status().backend.unwrap();
    assert_eq!((running.running, running.running_tasks), (true, 1));
    assert_eq!(h.stop().await, Ok(()));
    let status = h.hub.handle().status();
    assert_eq!(status.state, Some(BridgeState::Stopping));
    let stopped = status.backend.unwrap();
    assert_eq!((stopped.running, stopped.running_tasks), (false, 0));
}

#[tokio::test(start_paused = true)]
async fn events_and_status_carry_no_message_content() {
    const SECRET: &str = "s3cr3t";
    let mut h = Harness::started().await;
    h.messenger.owner_says(&format!("do {SECRET}"));
    h.backend.wait_for("prompt", 1).await;
    h.backend.turn_started(TurnSource::Prompt);
    h.backend
        .emit(BackendEvent::ThinkingDelta(format!("think {SECRET}")));
    h.backend.text(&format!("reply {SECRET}"));
    h.backend.emit(BackendEvent::ToolStarted {
        id: "tool-1".into(),
        tool: format!("Bash {SECRET}"),
        target: format!("{SECRET}/file"),
        task: Some(format!("task {SECRET}")),
    });
    settle().await;
    let permission = h.backend.request(RequestKind::Permission {
        tool: "Bash".into(),
        preview: format!("rm {SECRET}"),
    });
    h.card(1).await;
    h.messenger.owner_says("y");
    assert_eq!(permission.await, Ok(Answer::Permission(true)));
    let question = h.backend.request(RequestKind::Question {
        question: format!("Which {SECRET}?"),
        options: vec![format!("{SECRET} A"), "B".into()],
        multi_select: false,
        allow_free_text: true,
    });
    h.card(2).await;
    h.messenger.owner_says(&format!("answer {SECRET}"));
    assert!(question.await.is_ok());
    h.backend
        .emit(BackendEvent::Error(format!("error {SECRET}")));
    h.backend.emit(BackendEvent::TaskStarted {
        id: "task-1".into(),
        description: format!("task {SECRET}"),
        background: true,
    });
    h.backend.emit(BackendEvent::TaskCompleted {
        id: "task-1".into(),
        description: format!("task {SECRET}"),
        status: TaskStatus::Completed,
        background: true,
        report: Some(format!("report {SECRET}")),
    });
    h.backend.turn_ended(false);
    h.idle().await;
    h.backend.next_prompt(Err(BackendError {
        method: "session/prompt".into(),
        code: "-32000".into(),
        message: format!("failed {SECRET}"),
        kind: ErrorKind::Other,
    }));
    h.messenger.owner_says(&format!("again {SECRET}"));
    h.reply_containing(&format!("failed {SECRET}")).await;
    settle().await;

    let events = h.control_events();
    assert!(events.contains(&ControlEvent::TurnStarted {
        source: "prompt".into()
    }));
    assert!(events.contains(&ControlEvent::TurnEnded { interrupted: false }));
    assert!(events.contains(&ControlEvent::RequestClosed {
        kind: "question".into(),
        outcome: "answered".into(),
        open: 0,
    }));
    assert!(events.contains(&ControlEvent::Error {
        code: "-32000".into(),
        method: Some("session/prompt".into()),
    }));
    for event in &events {
        let line = serde_json::to_string(event).unwrap();
        assert!(!line.contains(SECRET), "{line}");
    }
    let status = serde_json::to_string(&h.hub.handle().status()).unwrap();
    assert!(!status.contains(SECRET), "{status}");
    assert!(
        status.contains("\"lastError\":{\"code\":\"-32000\""),
        "{status}"
    );
}

const UPGRADING: &str = "Test is being upgraded; your messages will be sent when it is back.";
const PAUSED_COMMAND: &str = "Test is paused for an upgrade; try again when it is back.";
const STOPPED_UNUSABLE: &str =
    "(Test stopped and its program is missing or in use; waiting for it to come back)";
const LOST: &str = "Undelivered background messages and in-memory schedules were lost.";

fn prepared(interrupted_tasks: usize) -> Prepared {
    Prepared {
        drained: true,
        interrupted_tasks,
        ..Prepared::default()
    }
}

/// The upgraded backend's start: the same session at `version`.
fn upgraded(version: &str, restored: Option<Restored>) -> Started {
    Started {
        session_id: "session-1".into(),
        engine_instance: Some("engine-1".into()),
        version: Some(version.into()),
        restored,
    }
}

fn paused_activity() -> (ActivityState, Option<String>) {
    (
        ActivityState::Working,
        Some("Paused: upgrading Test".to_owned()),
    )
}

#[tokio::test(start_paused = true)]
async fn a_pause_during_a_turn_waits_for_it_and_holds_owner_text() {
    let h = Harness::started().await;
    h.program("build one");
    h.messenger.owner_says("work");
    h.backend.wait_for("prompt work", 1).await;
    h.backend.turn_started(TurnSource::Prompt);
    let answer = h.backend.request(permission());
    h.card(1).await;
    let release = h.backend.hold_next_prepare();
    h.backend.next_prepare(Ok(prepared(0)));
    let pausing = h.pause(None);
    h.backend.wait_for("prepare drain=600000", 1).await;
    settle().await;
    assert!(!h.messenger.activities().contains(&paused_activity()));
    assert_eq!(h.hub.handle().status().state, Some(BridgeState::Paused));

    h.messenger.owner_says("y");
    assert_eq!(answer.await, Ok(Answer::Permission(true)));
    settle().await;
    assert_eq!(h.messenger.activities().last(), Some(&paused_activity()));
    h.messenger.owner_says("first");
    h.reply_containing(UPGRADING).await;
    h.messenger.owner_says("second");
    h.messenger.owner_says("/compact");
    h.reply_containing(PAUSED_COMMAND).await;
    h.messenger.owner_says("/status");
    h.backend.wait_for("status", 1).await;
    settle().await;
    let told = h
        .replies()
        .iter()
        .filter(|reply| *reply == UPGRADING)
        .count();
    assert_eq!(told, 1);
    assert_eq!(h.count("steer") + h.count("command") + h.count("prompt"), 1);
    let status = h.hub.handle().status();
    assert_eq!(status.held_texts, 2);
    assert_eq!(h.pause_phase().as_deref(), Some("draining"));

    h.backend.turn_ended(false);
    settle().await;
    assert_eq!(h.count("stop"), 0);
    assert!(!pausing.is_finished());
    drop(release);
    assert_eq!(
        pausing.await.unwrap(),
        Ok(json!({"state": "paused", "prepared": true, "drained": true, "interruptedTasks": 0}))
    );
    assert_eq!(h.count("stop"), 1);
    assert!(
        h.replies().iter().all(|reply| !reply.contains("Declined")),
        "{:?}",
        h.replies()
    );
    assert_eq!(h.pause_phase().as_deref(), Some("released"));
    assert_eq!(h.count("prompt"), 1);
}

#[tokio::test(start_paused = true)]
async fn after_the_file_changes_it_resumes_with_restore_and_announces() {
    let mut h = Harness::started().await;
    let program = h.program("build one");
    h.backend.next_prepare(Ok(prepared(1)));
    assert_eq!(
        h.pause(None).await.unwrap(),
        Ok(json!({"state": "paused", "prepared": true, "drained": true, "interruptedTasks": 1}))
    );
    h.messenger.owner_says("first");
    h.reply_containing(UPGRADING).await;
    h.messenger.owner_says("second");
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(h.count("start"), 1);
    assert_eq!(h.hub.handle().status().state, Some(BridgeState::Paused));

    h.backend.next_start(Ok(upgraded(
        "1.1",
        Some(Restored {
            schedules: 2,
            messages: 1,
            settings: vec!["model".into(), "goal".into()],
            interrupted_tasks: 1,
            ..Restored::default()
        }),
    )));
    std::fs::write(&program, "build number two").unwrap();
    h.backend.wait_for("start", 2).await;
    assert_eq!(
        h.starts()[1],
        format!("{} restore=true", h.start_call(&h.work, "session-1"))
    );
    h.reply_containing("Test upgraded").await;
    assert!(
        h.replies().contains(
            &"Test upgraded 1.0 → 1.1: 2 schedules, 1 message and the model/goal restored; 1 \
          interrupted task reported to Test."
                .to_owned()
        )
    );
    h.backend.wait_for("prompt first\n\nsecond", 1).await;
    settle().await;
    let status = h.hub.handle().status();
    assert_eq!((status.pause, status.held_texts), (None, 0));
    assert_eq!(status.state, Some(BridgeState::Idle));
    assert_eq!(
        status
            .backend
            .and_then(|backend| backend.version)
            .as_deref(),
        Some("1.1")
    );
    let states = Harness::states(&h.control_events());
    assert!(states.contains(&BridgeState::Paused), "{states:?}");
    assert_eq!(states.last(), Some(&BridgeState::Idle));
}

#[tokio::test(start_paused = true)]
async fn bypass_needs_a_new_confirmation_after_an_upgrade() {
    let h = Harness::started().await;
    h.messenger.owner_says("/mode bypass");
    h.card(1).await;
    h.messenger.owner_says("y");
    h.reply_containing("Approvals are off until /new.").await;
    let program = h.program("build one");
    h.backend.next_prepare(Ok(prepared(0)));
    h.pause(None).await.unwrap().unwrap();
    h.backend.next_start(Ok(upgraded(
        "1.1",
        Some(Restored {
            settings: vec![
                "bypassPermissions".into(),
                "permissionMode".into(),
                "model".into(),
            ],
            ..Default::default()
        }),
    )));
    std::fs::write(&program, "build number two").unwrap();
    h.backend.wait_for("start", 2).await;
    assert_eq!(
        h.starts()[1],
        format!("{} restore=true", h.start_call(&h.work, "session-1"))
    );
    h.card(2).await;
    let cards = h.cards();
    assert_eq!(cards[1].1, cards[0].1, "the bypass card again");
    let replies = h.replies();
    let announced = replies
        .iter()
        .position(|reply| reply == "Test upgraded 1.0 → 1.1: the model restored.")
        .expect("announced");
    assert_eq!(
        replies[announced + 1],
        "Approvals were off before the upgrade; confirm to turn them off again."
    );
    assert!(
        replies
            .iter()
            .all(|reply| !reply.contains("bypassPermissions") && !reply.contains("permissionMode")),
        "{replies:?}"
    );
    h.messenger.owner_says("y");
    h.backend
        .wait_for("command SetPermissionMode(bypassPermissions)", 2)
        .await;
}

/// Approvals off, then a pause that saved the state; returns the program file.
async fn bypassed_and_paused(h: &Harness) -> PathBuf {
    h.messenger.owner_says("/mode bypass");
    h.card(1).await;
    h.messenger.owner_says("y");
    h.reply_containing("Approvals are off until /new.").await;
    let program = h.program("build one");
    h.backend.next_prepare(Ok(prepared(0)));
    h.pause(None).await.unwrap().unwrap();
    program
}

/// The upgrade was announced and approvals are to be confirmed again.
async fn announced_with_the_bypass_card(h: &Harness) {
    h.card(2).await;
    let cards = h.cards();
    assert_eq!(cards[1].1, cards[0].1, "the bypass card again");
    let replies = h.replies();
    let announced = replies
        .iter()
        .position(|reply| reply == "Test upgraded 1.0 → 1.1: nothing to restore.")
        .unwrap_or_else(|| panic!("announced: {replies:?}"));
    assert_eq!(
        replies[announced + 1],
        "Approvals were off before the upgrade; confirm to turn them off again."
    );
    assert!(
        replies
            .iter()
            .all(|reply| !reply.starts_with("Test restarted")),
        "{replies:?}"
    );
}

/// Pauses (the backend saves its state) and resumes into `started` once the program
/// file changed.
async fn upgraded_into(h: &Harness, started: Started) {
    let program = h.program("build one");
    h.backend.next_prepare(Ok(prepared(0)));
    h.pause(None).await.unwrap().unwrap();
    h.backend.next_start(Ok(started));
    std::fs::write(&program, "build number two").unwrap();
    h.backend.wait_for("start", 2).await;
}

#[tokio::test(start_paused = true)]
async fn the_pause_tells_the_backend_the_last_message_shown() {
    let h = Harness::started().await;
    h.backend.emit(BackendEvent::AgentMessage {
        cursor: 3,
        label: "Note".into(),
        text: "hello".into(),
        urgent: false,
    });
    h.reply_containing("hello").await;
    h.backend.next_prepare(Ok(prepared(0)));
    h.pause(None).await.unwrap().unwrap();
    assert_eq!(h.count("prepare drain=600000 delivered=3"), 1);
}

#[tokio::test(start_paused = true)]
async fn the_announcement_prefers_the_versions_coda_restored_from_and_to() {
    let h = Harness::started().await;
    let mut started = upgraded("coda (contract 2026-09-1)", None);
    started.restored = Some(Restored {
        from_version: Some("0.1.176".into()),
        to_version: Some("0.1.177".into()),
        ..Restored::default()
    });
    upgraded_into(&h, started).await;
    h.reply_containing("Test upgraded 0.1.176 → 0.1.177: nothing to restore.")
        .await;
}

#[tokio::test(start_paused = true)]
async fn the_announcement_drops_the_program_name_from_versions() {
    let mut h = Harness::new();
    h.backend
        .next_start(Ok(upgraded("coda 0.1.176 (contract 2026-09-1)", None)));
    h.start();
    h.backend.wait_for("start", 1).await;
    settle().await;
    upgraded_into(
        &h,
        upgraded(
            "coda 0.1.177 (contract 2026-09-1)",
            Some(Restored::default()),
        ),
    )
    .await;
    h.reply_containing("Test upgraded 0.1.176 → 0.1.177: nothing to restore.")
        .await;
}

#[tokio::test(start_paused = true)]
async fn a_saved_state_that_was_not_applied_is_announced_as_lost() {
    let h = Harness::started().await;
    let restored = Restored {
        applied: false,
        ignored_reason: Some("expired".into()),
        ..Restored::default()
    };
    upgraded_into(&h, upgraded("1.1", Some(restored))).await;
    h.reply_containing(&format!(
        "Test upgraded 1.0 → 1.1; its saved state was not restored (expired). {LOST}"
    ))
    .await;
    assert!(
        h.replies()
            .iter()
            .all(|reply| !reply.contains(" restored:")),
        "{:?}",
        h.replies()
    );
}

fn bypass_not_restored() -> Restored {
    Restored {
        settings_not_restored: vec![NotRestored {
            name: "permissionMode".into(),
            value: Some("bypassPermissions".into()),
            reason: None,
        }],
        ..Restored::default()
    }
}

#[tokio::test(start_paused = true)]
async fn bypass_reported_not_restored_asks_for_a_confirmation() {
    let h = Harness::started().await;
    upgraded_into(&h, upgraded("1.1", Some(bypass_not_restored()))).await;
    h.card(1).await;
    let bypass = cards::bypass_card("Test").card.expect("a card");
    assert_eq!(h.cards()[0].1, template(&bypass));
    h.reply_containing("Approvals were off before the upgrade")
        .await;
}

#[tokio::test(start_paused = true)]
async fn bypass_known_twice_asks_for_one_confirmation() {
    let h = Harness::started().await;
    let program = bypassed_and_paused(&h).await;
    h.backend
        .next_start(Ok(upgraded("1.1", Some(bypass_not_restored()))));
    std::fs::write(&program, "build number two").unwrap();
    h.backend.wait_for("start", 2).await;
    announced_with_the_bypass_card(&h).await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(h.cards().len(), 2);
    let asked = h
        .replies()
        .iter()
        .filter(|reply| reply.starts_with("Approvals were off"))
        .count();
    assert_eq!(asked, 1);
}

#[tokio::test(start_paused = true)]
async fn a_failed_resume_keeps_the_restore_and_the_bypass_confirmation() {
    let h = Harness::started().await;
    let program = bypassed_and_paused(&h).await;
    h.backend.fail_next_start(ErrorKind::Other);
    h.backend
        .next_start(Ok(upgraded("1.1", Some(Restored::default()))));
    std::fs::write(&program, "build number two").unwrap();
    h.backend.wait_for("start", 2).await;
    h.reply_containing("retrying").await;
    h.backend.wait_for("start", 3).await;
    let restoring = format!("{} restore=true", h.start_call(&h.work, "session-1"));
    assert_eq!(h.starts()[1..], [restoring.clone(), restoring]);
    announced_with_the_bypass_card(&h).await;
}

#[tokio::test(start_paused = true)]
async fn a_session_the_owner_picks_drops_a_failed_resume() {
    for (command, reply) in [
        ("/new", "New session"),
        ("/cd ../other", "Folder "),
        ("/resume s-7", "Resumed session s-7."),
    ] {
        let h = Harness::started().await;
        let program = bypassed_and_paused(&h).await;
        h.backend.fail_next_start(ErrorKind::Other);
        std::fs::write(&program, "build number two").unwrap();
        h.backend.wait_for("start", 2).await;
        h.reply_containing("retrying").await;
        h.messenger.owner_says(command);
        h.reply_containing(reply).await;
        h.backend.wait_for("start", 3).await;
        tokio::time::sleep(Duration::from_secs(3_600)).await;
        settle().await;
        assert_eq!(h.count("start"), 3, "{command}: no start from the timer");
        assert_eq!(h.cards().len(), 1, "{command}: no bypass card");
        let replies = h.replies();
        assert!(
            replies
                .iter()
                .all(|reply| !reply.contains("upgraded") && !reply.contains("restarted")),
            "{command}: {replies:?}"
        );
        h.messenger.owner_says("/status");
        h.reply_containing("Permission mode: default").await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_resume_failing_on_a_missing_program_keeps_the_restore_and_the_bypass_confirmation() {
    let h = Harness::started().await;
    let program = bypassed_and_paused(&h).await;
    let release = h.backend.hold_next_start();
    h.backend.fail_next_start(ErrorKind::Other);
    h.backend
        .next_start(Ok(upgraded("1.1", Some(Restored::default()))));
    std::fs::write(&program, "build number two").unwrap();
    h.backend.wait_for("start", 2).await;
    std::fs::remove_file(&program).unwrap();
    drop(release);
    h.reply_containing(STOPPED_UNUSABLE).await;
    settle().await;
    assert_eq!(h.pause_phase().as_deref(), Some("waiting_for_file"));
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(h.count("start"), 2);
    std::fs::write(&program, "build number three").unwrap();
    h.backend.wait_for("start", 3).await;
    let restoring = format!("{} restore=true", h.start_call(&h.work, "session-1"));
    assert_eq!(h.starts()[1..], [restoring.clone(), restoring]);
    announced_with_the_bypass_card(&h).await;
}

#[tokio::test(start_paused = true)]
async fn the_fallback_works_without_prepare_restart() {
    let h = Harness::started().await;
    let program = h.program("build one");
    h.messenger.owner_says("work");
    h.backend.wait_for("prompt work", 1).await;
    h.backend.turn_started(TurnSource::Prompt);
    let pausing = h.pause(None);
    h.backend.wait_for("prepare", 1).await;
    settle().await;
    assert_eq!(h.count("stop"), 0);
    h.messenger.owner_says("held");
    h.reply_containing(UPGRADING).await;
    h.backend.turn_ended(false);
    assert_eq!(
        pausing.await.unwrap(),
        Ok(json!({"state": "paused", "prepared": false, "drained": true, "interruptedTasks": 0}))
    );
    assert_eq!(h.count("stop"), 1);
    h.backend.next_start(Ok(upgraded("1.1", None)));
    std::fs::write(&program, "build number two").unwrap();
    h.backend.wait_for("start", 2).await;
    assert_eq!(h.starts()[1], h.start_call(&h.work, "session-1"));
    h.reply_containing(&format!("Test upgraded 1.0 → 1.1. {LOST}"))
        .await;
    h.backend.wait_for("prompt held", 1).await;
}

#[tokio::test(start_paused = true)]
async fn the_fallback_cuts_a_turn_at_the_drain_deadline() {
    let h = Harness::started().await;
    h.program("build one");
    h.messenger.owner_says("work");
    h.backend.wait_for("prompt work", 1).await;
    h.backend.turn_started(TurnSource::Prompt);
    h.backend.text("partial");
    let answer = h.backend.request(permission());
    h.card(1).await;
    let pausing = h.pause(Some(5_000));
    h.backend.wait_for("prepare drain=5000", 1).await;
    assert_eq!(
        pausing.await.unwrap(),
        Ok(json!({"state": "paused", "prepared": false, "drained": false, "interruptedTasks": 0}))
    );
    assert!(answer.await.is_err(), "declined");
    h.reply_containing("Declined: Test is being upgraded.")
        .await;
    h.reply_containing("(Test paused for an upgrade)").await;
    assert_eq!(h.count("stop"), 1);
    assert_eq!(h.hub.handle().status().open_requests, 0);
}

#[cfg(windows)]
#[tokio::test(start_paused = true)]
async fn a_killed_coda_with_a_locked_exe_pauses_instead_of_restarting() {
    use std::os::windows::fs::OpenOptionsExt;

    let h = Harness::started().await;
    let program = h.program("build one");
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&program)
        .unwrap();
    h.backend.emit(exited());
    h.reply_containing(STOPPED_UNUSABLE).await;
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(h.count("start"), 1);
    assert_eq!(h.hub.handle().status().state, Some(BridgeState::Paused));
    assert_eq!(h.pause_phase().as_deref(), Some("waiting_for_file"));
    drop(held);
    h.backend.wait_for("start", 2).await;
    assert_eq!(h.starts()[1], h.start_call(&h.work, "session-1"));
    h.reply_containing(&format!("Test restarted. {LOST}")).await;
    settle().await;
    assert_eq!(h.hub.handle().status().state, Some(BridgeState::Idle));
}

#[tokio::test(start_paused = true)]
async fn a_killed_coda_with_a_missing_exe_pauses_instead_of_restarting() {
    let h = Harness::started().await;
    let program = h.program("build one");
    std::fs::remove_file(&program).unwrap();
    h.backend.emit(exited());
    h.reply_containing(STOPPED_UNUSABLE).await;
    h.messenger.owner_says("meanwhile");
    h.reply_containing(UPGRADING).await;
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(h.count("start"), 1);
    assert_eq!(h.hub.handle().status().state, Some(BridgeState::Paused));
    assert_eq!(h.pause_phase().as_deref(), Some("waiting_for_file"));
    assert_eq!(
        h.messenger.activities().last(),
        Some(&(
            ActivityState::Working,
            Some("Waiting for Test's program".to_owned())
        ))
    );
    std::fs::write(&program, "build two").unwrap();
    h.backend.wait_for("start", 2).await;
    assert_eq!(h.starts()[1], h.start_call(&h.work, "session-1"));
    h.reply_containing(&format!("Test restarted. {LOST}")).await;
    h.backend.wait_for("prompt meanwhile", 1).await;
}

#[tokio::test(start_paused = true)]
async fn a_failed_start_with_a_missing_exe_pauses_too() {
    let h = Harness::started().await;
    let program = h.program("build one");
    h.backend.emit(exited());
    h.reply_containing("(Test stopped; restarting)").await;
    std::fs::remove_file(&program).unwrap();
    h.backend.fail_next_start(ErrorKind::Other);
    h.backend.wait_for("start", 2).await;
    settle().await;
    assert_eq!(h.pause_phase().as_deref(), Some("waiting_for_file"));
    tokio::time::sleep(Duration::from_secs(300)).await;
    assert_eq!(h.count("start"), 2);
    std::fs::write(&program, "build one").unwrap();
    h.backend.wait_for("start", 3).await;
    settle().await;
    assert_eq!(h.hub.handle().status().state, Some(BridgeState::Idle));
}

#[tokio::test(start_paused = true)]
async fn resume_without_pause_is_an_error() {
    let h = Harness::started().await;
    let error = h.call(Action::Resume).await.unwrap_err();
    assert_eq!(error.code, "not_paused");
    assert_eq!(h.count("stop"), 0);
}

#[tokio::test(start_paused = true)]
async fn pause_is_idempotent_and_resume_restarts_at_once() {
    let h = Harness::started().await;
    h.program("build one");
    h.backend.next_prepare(Ok(prepared(0)));
    h.pause(None).await.unwrap().unwrap();
    assert_eq!(
        h.call(Action::Pause(PauseParams::default())).await,
        Ok(json!({"state": "paused", "phase": "released"}))
    );
    assert_eq!((h.count("prepare"), h.count("stop")), (1, 1));
    assert_eq!(h.call(Action::Resume).await, Ok(json!({"resumed": true})));
    h.backend.wait_for("start", 2).await;
    assert_eq!(
        h.starts()[1],
        format!("{} restore=true", h.start_call(&h.work, "session-1"))
    );
    h.reply_containing("Test restarted.").await;
    settle().await;
    assert_eq!(h.hub.handle().status().state, Some(BridgeState::Idle));
}

#[tokio::test(start_paused = true)]
async fn resume_waits_for_a_usable_program() {
    let h = Harness::started().await;
    let program = h.program("build one");
    h.backend.next_prepare(Ok(prepared(0)));
    h.pause(None).await.unwrap().unwrap();
    std::fs::remove_file(&program).unwrap();
    let error = h.call(Action::Resume).await.unwrap_err();
    assert_eq!(error.code, "exe_unavailable");
    assert_eq!(h.count("start"), 1);
    assert_eq!(h.pause_phase().as_deref(), Some("released"));
}

#[tokio::test(start_paused = true)]
async fn resume_while_draining_cancels_the_pause() {
    let h = Harness::started().await;
    h.program("build one");
    h.messenger.owner_says("work");
    h.backend.wait_for("prompt work", 1).await;
    h.backend.turn_started(TurnSource::Prompt);
    let release = h.backend.hold_next_prepare();
    h.backend.next_prepare(Ok(prepared(0)));
    let pausing = h.pause(None);
    h.backend.wait_for("prepare", 1).await;
    h.messenger.owner_says("held");
    h.reply_containing(UPGRADING).await;
    assert_eq!(
        h.call(Action::Resume).await,
        Ok(json!({"resumed": true, "cancelled": true}))
    );
    assert_eq!(pausing.await.unwrap().unwrap_err().code, "cancelled");
    settle().await;
    let status = h.hub.handle().status();
    assert_eq!(
        (status.pause, status.state),
        (None, Some(BridgeState::Working))
    );
    h.backend.turn_ended(false);
    h.backend.wait_for("prompt held", 1).await;
    drop(release);
    settle().await;
    assert_eq!(h.count("stop"), 0);
    // Coda saved its state: the next start restores it.
    h.backend.emit(exited());
    h.backend.wait_for("start", 2).await;
    assert_eq!(
        h.starts()[1],
        format!("{} restore=true", h.start_call(&h.work, "session-1"))
    );
}

#[tokio::test(start_paused = true)]
async fn a_pause_s_events_and_status_carry_no_held_text() {
    const SECRET: &str = "s3cr3t";
    let mut h = Harness::started().await;
    h.program("build one");
    h.backend.next_prepare(Ok(prepared(0)));
    h.pause(None).await.unwrap().unwrap();
    h.messenger.owner_says(&format!("do {SECRET}"));
    h.reply_containing(UPGRADING).await;
    settle().await;
    let status = h.hub.handle().status();
    assert_eq!(status.held_texts, 1);
    assert_eq!(
        status
            .pause
            .as_ref()
            .and_then(|pause| pause.reason.as_deref()),
        Some("upgrade")
    );
    let line = serde_json::to_string(&status).unwrap();
    assert!(!line.contains(SECRET), "{line}");
    for event in h.control_events() {
        let line = serde_json::to_string(&event).unwrap();
        assert!(!line.contains(SECRET), "{line}");
    }
}

/// The peek's entries now.
pub(super) fn peeked(h: &Harness) -> Vec<push_bridge_control::PeekEntry> {
    h.hub
        .handle()
        .peek(push_bridge_control::PEEK_CAP)
        .expect("the peek is on")
}

/// A turn with one finished tool step and `text`.
async fn turn_with_a_step(h: &Harness, target: &str, text: &str) {
    h.backend.turn_started(TurnSource::Prompt);
    h.backend.emit(BackendEvent::ToolStarted {
        id: "1".into(),
        tool: "Bash".into(),
        target: target.into(),
        task: None,
    });
    h.backend.emit(BackendEvent::ToolFinished {
        id: "1".into(),
        failed: false,
    });
    h.backend.text(text);
    h.backend.turn_ended(false);
    h.idle().await;
}

#[tokio::test(start_paused = true)]
async fn the_peek_follows_the_owner_conversation() {
    use push_bridge_control::{CardState, PeekFrom, PeekKind};
    let mut h = Harness::started().await;
    h.control_events();
    h.messenger.owner_says("hello api_key=hunter2");
    h.backend.wait_for("prompt hello", 1).await;
    let stranger = h.messenger.says(UserId::new(), "let me in");
    h.messenger.inject(Event::MessageAdded(stranger));
    settle().await;
    let entries = peeked(&h);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(
        (entries[0].from, entries[0].kind),
        (PeekFrom::Owner, PeekKind::Text)
    );
    assert!(entries[0].text.starts_with("hello api_key="), "{entries:?}");
    assert!(!entries[0].text.contains("hunter2"));
    assert!(h.control_events().contains(&ControlEvent::PeekChanged));

    turn_with_a_step(&h, "cargo test", "All green.").await;
    let entries = peeked(&h);
    let shown: Vec<_> = entries[1..]
        .iter()
        .map(|e| (e.from, e.kind, e.text.as_str()))
        .collect();
    assert_eq!(
        shown,
        [
            (PeekFrom::Agent, PeekKind::Step, "✓ Bash cargo test"),
            (PeekFrom::Agent, PeekKind::Reply, "All green."),
        ]
    );

    let answer = h.backend.request(permission());
    let card = h.card(1).await;
    let last = peeked(&h).pop().unwrap();
    assert_eq!(
        (last.kind, last.text.as_str(), last.card_state),
        (
            PeekKind::Card,
            "Allow Bash? [Approve] [Reject]",
            Some(CardState::Open)
        )
    );
    h.messenger.answer(card, &["approve"]);
    assert_eq!(answer.await, Ok(Answer::Permission(true)));
    let entries = peeked(&h);
    let answered = entries.last().unwrap();
    assert_eq!(
        (answered.from, answered.kind, answered.text.as_str()),
        (PeekFrom::Owner, PeekKind::Answer, "Approve")
    );
    let card_entry = &entries[entries.len() - 2];
    assert_eq!(card_entry.card_state, Some(CardState::Answered));

    h.messenger.owner_says("/status");
    h.reply_containing("Session:").await;
    let entries = peeked(&h);
    let tail: Vec<_> = entries[entries.len() - 2..]
        .iter()
        .map(|e| (e.from, e.kind))
        .collect();
    assert_eq!(
        tail,
        [
            (PeekFrom::Owner, PeekKind::Text),
            (PeekFrom::Agent, PeekKind::Note)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn the_peek_shows_expired_and_declined_cards() {
    use push_bridge_control::{CardState, PeekKind};
    let h = Harness::started().await;
    let first = h.backend.request(permission());
    h.card(1).await;
    tokio::time::sleep(cards::CARD_TIMEOUT + Duration::from_secs(1)).await;
    assert_eq!(first.await, Ok(Answer::Permission(false)));
    h.backend.request(permission());
    h.card(2).await;
    h.messenger.owner_says("/new");
    h.reply_containing("Declined: ").await;
    let states: Vec<_> = peeked(&h)
        .into_iter()
        .filter(|e| e.kind == PeekKind::Card)
        .map(|e| e.card_state)
        .collect();
    assert_eq!(
        states,
        [Some(CardState::Expired), Some(CardState::Declined)]
    );
}

#[tokio::test(start_paused = true)]
async fn the_peek_writes_nothing_to_disk() {
    const MARKER: &str = "zq-marker-7731";
    let mut h = Harness::started().await;
    h.messenger.owner_says(&format!("say {MARKER}"));
    h.backend.wait_for("prompt", 1).await;
    turn_with_a_step(&h, &format!("grep {MARKER}"), &format!("found {MARKER}")).await;
    assert!(peeked(&h).iter().any(|e| e.text.contains(MARKER)));
    h.stop().await.unwrap();
    let mut folders = vec![h._dir.path().to_path_buf()];
    let mut files = 0;
    while let Some(folder) = folders.pop() {
        for item in std::fs::read_dir(&folder).unwrap() {
            let path = item.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else {
                files += 1;
                let bytes = std::fs::read(&path).unwrap();
                let text = String::from_utf8_lossy(&bytes);
                assert!(!text.contains(MARKER), "{}", path.display());
            }
        }
    }
    assert!(files > 0, "the state file was written");
}
