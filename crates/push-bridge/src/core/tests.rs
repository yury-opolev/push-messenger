//! The core's behaviour against a [`TestBackend`] and a [`TestMessenger`], in paused
//! time. The end-to-end flows are in `tests/bridge.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use push_sdk::{ActivityState, ClientMsgId, ConnectionState, Event, MessageKind, SdkError, UserId};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::*;
use crate::backend::{
    Answer, BackendStatus, ErrorKind, PendingMessages, RequestKind, SteerOutcome, TaskStatus,
    TurnSource,
};
use crate::folders;
use crate::messenger::{Call, TestMessenger, param, template};
use crate::test_backend::{TestBackend, error};
use crate::test_dir::TestDir;

type Channels = (mpsc::UnboundedReceiver<BackendEvent>, mpsc::Receiver<Event>);

struct Harness {
    messenger: Arc<TestMessenger>,
    backend: Arc<TestBackend>,
    state: Arc<StateFile>,
    root: PathBuf,
    work: PathBuf,
    other: PathBuf,
    channels: Option<Channels>,
    stop: Option<oneshot::Sender<()>>,
    run: Option<JoinHandle<Result<(), BridgeError>>>,
    _dir: TestDir,
}

impl Harness {
    fn new() -> Self {
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
        let (backend, backend_events) = TestBackend::channel();
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
            _dir: dir,
        }
    }

    fn start(&mut self) {
        let (backend_events, messenger_events) = self.channels.take().expect("not started");
        let bridge = Bridge::new(
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
        );
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

    async fn stop(&mut self) -> Result<(), BridgeError> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.run.take().expect("running").await.unwrap()
    }

    fn replies(&self) -> Vec<String> {
        self.messenger.reply_texts()
    }

    /// The cards sent (not replies): `(id, template)`.
    fn cards(&self) -> Vec<(ClientMsgId, String)> {
        self.messenger
            .sent_rich()
            .into_iter()
            .map(|(id, body)| (id, template(&body)))
            .filter(|(_, tpl)| tpl != "agent.reply@1")
            .collect()
    }

    /// The `count`th card, once sent.
    async fn card(&self, count: usize) -> ClientMsgId {
        self.messenger
            .wait_until("a card", |_| self.cards().len() >= count)
            .await;
        self.cards()[count - 1].0
    }

    async fn reply_containing(&self, text: &str) {
        self.messenger
            .wait_until(text, |_| {
                self.replies().iter().any(|reply| reply.contains(text))
            })
            .await;
    }

    async fn idle(&self) {
        self.messenger
            .wait_until("idle", |_| {
                self.messenger.activities().last() == Some(&(ActivityState::Idle, None))
            })
            .await;
    }

    fn count(&self, prefix: &str) -> usize {
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
}

/// Lets the core handle everything waiting: paused time moves on only when every task
/// is idle.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

fn permission() -> RequestKind {
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
async fn busy_then_no_active_turn_prompts_again() {
    let h = Harness::started().await;
    h.backend
        .next_prompt(Err(error("session/prompt", ErrorKind::Busy)));
    h.backend.next_steer(Ok(SteerOutcome::NoActiveTurn));
    h.messenger.owner_says("go");
    h.backend.wait_for("prompt go", 2).await;
    assert_eq!(
        h.backend.calls()[1..],
        ["prompt go", "steer go", "prompt go"]
    );
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
    assert!(param(&body, "text").unwrap().ends_with("(stopped)"));
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
    assert!(status.contains(&format!("{}work\n", std::path::MAIN_SEPARATOR)));
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
