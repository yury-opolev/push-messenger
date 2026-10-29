use std::future::Future;
use std::path::PathBuf;

use super::*;
use crate::coda::testing::{FakeCoda, FakeLauncher};

const WAIT: Duration = Duration::from_secs(5);

async fn within<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(WAIT, future).await.expect("timed out")
}

fn options(folder: &str, session: Option<&str>, mode: PermissionMode) -> StartOptions {
    StartOptions {
        folder: PathBuf::from(folder),
        session: session.map(str::to_owned),
        permission_mode: mode,
    }
}

struct Harness {
    backend: Arc<CodaBackend>,
    events: mpsc::UnboundedReceiver<BackendEvent>,
    launcher: Arc<FakeLauncher>,
    fakes: mpsc::UnboundedReceiver<FakeCoda>,
}

fn harness() -> Harness {
    let (launcher, fakes) = FakeLauncher::new();
    let (sender, events) = mpsc::unbounded_channel();
    let backend = Arc::new(CodaBackend::with_launcher(launcher.clone(), sender));
    Harness {
        backend,
        events,
        launcher,
        fakes,
    }
}

impl Harness {
    async fn event(&mut self) -> BackendEvent {
        within(self.events.recv()).await.expect("an event")
    }

    async fn no_event(&mut self) {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(event) = self.events.try_recv() {
            panic!("unexpected event {event:?}");
        }
    }

    /// Starts in a new process answered by `handshake`; returns the result, the fake
    /// and the initialize params.
    async fn spawn_start(
        &mut self,
        options: StartOptions,
    ) -> (Result<Started, BackendError>, FakeCoda, Value) {
        let backend = Arc::clone(&self.backend);
        let start = tokio::spawn(async move { backend.start(options).await });
        let mut fake = within(self.fakes.recv()).await.expect("a spawn");
        let params = within(fake.handshake("s-1")).await;
        (within(start).await.unwrap(), fake, params)
    }

    async fn running(&mut self) -> FakeCoda {
        let (started, fake, _) = self
            .spawn_start(options("/work/a", None, PermissionMode::Default))
            .await;
        started.unwrap();
        fake
    }

    fn prompt(&self, text: &str) -> tokio::task::JoinHandle<Result<(), BackendError>> {
        let backend = Arc::clone(&self.backend);
        let text = text.to_owned();
        tokio::spawn(async move { backend.prompt(&text).await })
    }

    fn command(
        &self,
        command: BackendCommand,
    ) -> tokio::task::JoinHandle<Result<CommandReply, BackendError>> {
        let backend = Arc::clone(&self.backend);
        tokio::spawn(async move { backend.command(command).await })
    }
}

#[tokio::test]
async fn initialize_negotiates_state_events_and_reports_the_session() {
    let mut h = harness();
    let (started, fake, params) = h
        .spawn_start(options("/work/a", None, PermissionMode::Default))
        .await;
    assert_eq!(
        started.unwrap(),
        Started {
            session_id: "s-1".into(),
            engine_instance: Some("engine-1".into()),
            version: Some("coda (contract 2026-09-1)".into()),
        }
    );
    assert_eq!(params["protocolVersion"], "1");
    assert_eq!(params["clientInfo"], CLIENT_INFO);
    assert_eq!(params["clientCapabilities"], json!({ "stateEvents": true }));
    assert!(params.get("sessionId").is_none(), "{params}");
    assert_eq!(
        h.launcher.spawns(),
        vec![(PathBuf::from("/work/a"), PermissionMode::Default)]
    );
    assert_eq!(fake.mode, PermissionMode::Default);
}

#[tokio::test]
async fn initialize_resumes_a_session() {
    let mut h = harness();
    let (started, _fake, params) = h
        .spawn_start(options("/work/a", Some("abc"), PermissionMode::Plan))
        .await;
    started.unwrap();
    assert_eq!(params["sessionId"], "abc");
    assert_eq!(params["clientCapabilities"]["stateEvents"], true);
    assert_eq!(
        h.launcher.spawns(),
        vec![(PathBuf::from("/work/a"), PermissionMode::Plan)]
    );
}

#[tokio::test]
async fn an_unknown_session_is_session_not_found_and_the_process_stays() {
    let mut h = harness();
    let mut fake = h.running().await;
    let backend = Arc::clone(&h.backend);
    let start = tokio::spawn(async move {
        backend
            .start(options("/work/a", Some("gone"), PermissionMode::Default))
            .await
    });
    let (id, params) = within(fake.expect("initialize")).await;
    assert_eq!(params["sessionId"], "gone");
    fake.respond_error(&id, -32002, "session not found").await;
    let error = within(start).await.unwrap().unwrap_err();
    assert_eq!(error.kind, ErrorKind::SessionNotFound);
    assert_eq!(
        (error.method.as_str(), error.code.as_str()),
        ("initialize", "-32002")
    );
    assert_eq!(h.launcher.spawns().len(), 1);
}

#[tokio::test]
async fn a_prompt_maps_its_turn_events_in_order() {
    let mut h = harness();
    let mut fake = h.running().await;
    let prompt = h.prompt("fix the bug");
    let (id, params) = within(fake.expect("session/prompt")).await;
    assert_eq!(params, json!({ "text": "fix the bug" }));
    for (method, params) in [
        (
            "event/activity",
            json!({"turnId": "t1", "phase": "preparing", "phaseSince": "x"}),
        ),
        ("event/thinking", json!({"delta": "hmm"})),
        (
            "event/activity",
            json!({"turnId": "t1", "phase": "responding"}),
        ),
        ("event/assistantText", json!({"delta": "Looking"})),
        (
            "event/toolCall",
            json!({"toolName": "read_file", "inputJson": "{\"path\":\"src/main.rs\",\"limit\":5}", "callId": "c1"}),
        ),
        (
            "event/toolResult",
            json!({"toolName": "read_file", "callId": "c1", "isError": false, "status": "Succeeded", "content": "…"}),
        ),
        ("event/turnComplete", json!({"interrupted": false})),
        (
            "event/turnEnded",
            json!({"turnId": "t1", "interrupted": false}),
        ),
    ] {
        fake.notify(method, params).await;
    }
    fake.respond(&id, json!({"ok": true, "interrupted": false}))
        .await;
    within(prompt).await.unwrap().unwrap();

    let event = h.event().await;
    assert!(
        matches!(
            event,
            BackendEvent::TurnStarted {
                source: TurnSource::Prompt
            }
        ),
        "{event:?}"
    );
    let event = h.event().await;
    assert!(
        matches!(&event, BackendEvent::ThinkingDelta(delta) if delta == "hmm"),
        "{event:?}"
    );
    let event = h.event().await;
    assert!(
        matches!(&event, BackendEvent::TextDelta(delta) if delta == "Looking"),
        "{event:?}"
    );
    let BackendEvent::ToolStarted {
        id,
        tool,
        target,
        task,
    } = h.event().await
    else {
        panic!("a tool start");
    };
    assert_eq!(
        (id.as_str(), tool.as_str(), target.as_str()),
        ("c1", "read_file", "src/main.rs")
    );
    assert_eq!(task, None);
    let event = h.event().await;
    assert!(
        matches!(&event, BackendEvent::ToolFinished { id, failed: false } if id == "c1"),
        "{event:?}"
    );
    let event = h.event().await;
    assert!(
        matches!(event, BackendEvent::TurnEnded { interrupted: false }),
        "{event:?}"
    );
    h.no_event().await;
}

#[tokio::test]
async fn a_turn_the_engine_starts_itself_is_marked_engine() {
    let mut h = harness();
    let mut fake = h.running().await;
    fake.notify(
        "event/activity",
        json!({"turnId": "c9", "phase": "compacting"}),
    )
    .await;
    fake.notify(
        "event/turnEnded",
        json!({"turnId": "c9", "interrupted": false}),
    )
    .await;
    fake.notify(
        "event/activity",
        json!({"turnId": "t2", "phase": "preparing"}),
    )
    .await;
    fake.notify(
        "event/turnEnded",
        json!({"turnId": "t2", "interrupted": true}),
    )
    .await;
    let event = h.event().await;
    assert!(
        matches!(
            event,
            BackendEvent::TurnStarted {
                source: TurnSource::Engine
            }
        ),
        "{event:?}"
    );
    let event = h.event().await;
    assert!(
        matches!(event, BackendEvent::TurnEnded { interrupted: true }),
        "{event:?}"
    );
    h.no_event().await;
}

#[tokio::test]
async fn without_state_events_turns_follow_the_legacy_events() {
    let mut h = harness();
    let backend = Arc::clone(&h.backend);
    let start = tokio::spawn(async move {
        backend
            .start(options("/work/a", None, PermissionMode::Default))
            .await
    });
    let mut fake = within(h.fakes.recv()).await.unwrap();
    let (id, _) = within(fake.expect("initialize")).await;
    fake.respond(
        &id,
        json!({"protocolVersion": "1", "sessionId": "s-1", "serverInfo": "coda"}),
    )
    .await;
    within(start).await.unwrap().unwrap();
    let prompt = h.prompt("hi");
    let (id, _) = within(fake.expect("session/prompt")).await;
    fake.notify("event/assistantText", json!({"delta": "Hello"}))
        .await;
    fake.notify("event/turnComplete", json!({"interrupted": false}))
        .await;
    fake.respond(&id, json!({"ok": true})).await;
    within(prompt).await.unwrap().unwrap();
    let event = h.event().await;
    assert!(
        matches!(
            event,
            BackendEvent::TurnStarted {
                source: TurnSource::Prompt
            }
        ),
        "{event:?}"
    );
    assert!(matches!(h.event().await, BackendEvent::TextDelta(_)));
    assert!(matches!(
        h.event().await,
        BackendEvent::TurnEnded { interrupted: false }
    ));
}

#[tokio::test]
async fn a_prompt_while_busy_is_busy() {
    let mut h = harness();
    let mut fake = h.running().await;
    let prompt = h.prompt("again");
    let (id, _) = within(fake.expect("session/prompt")).await;
    fake.respond_error(&id, -32603, "busy: a turn is already running")
        .await;
    let error = within(prompt).await.unwrap().unwrap_err();
    assert_eq!(error.kind, ErrorKind::Busy);
    assert_eq!(
        (error.method.as_str(), error.code.as_str()),
        ("session/prompt", "-32603")
    );
}

#[tokio::test]
async fn steer_outcomes() {
    let mut h = harness();
    let mut fake = h.running().await;
    for (result, outcome) in [
        (
            json!({"ok": true, "messageId": "m1"}),
            SteerOutcome::Accepted,
        ),
        (
            json!({"ok": false, "rejectedReason": "noActiveTurn"}),
            SteerOutcome::NoActiveTurn,
        ),
        (
            json!({"ok": false, "rejectedReason": "turnEnding"}),
            SteerOutcome::TurnEnding,
        ),
        (
            json!({"ok": false, "rejectedReason": "emptyText"}),
            SteerOutcome::Rejected("emptyText".into()),
        ),
    ] {
        let backend = Arc::clone(&h.backend);
        let steer = tokio::spawn(async move { backend.steer("also this").await });
        let (id, params) = within(fake.expect("session/steer")).await;
        assert_eq!(params, json!({"text": "also this"}));
        fake.respond(&id, result).await;
        assert_eq!(within(steer).await.unwrap().unwrap(), outcome);
    }
}

async fn raise(
    h: &mut Harness,
    fake: &mut FakeCoda,
    method: &str,
    params: Value,
) -> (Value, BackendRequest) {
    let id = fake.send_request(method, params).await;
    let BackendEvent::Request(request) = h.event().await else {
        panic!("a request");
    };
    (id, request)
}

#[tokio::test]
async fn answers_reach_coda() {
    let mut h = harness();
    let mut fake = h.running().await;

    let (id, request) = raise(
        &mut h,
        &mut fake,
        "request/permission",
        json!({"toolName": "run_command", "inputPreview": "cargo test", "requestId": "req-1"}),
    )
    .await;
    assert_eq!(
        request.kind,
        RequestKind::Permission {
            tool: "run_command".into(),
            preview: "cargo test".into()
        }
    );
    request.responder.answer(Answer::Permission(true));
    assert_eq!(
        within(fake.response(&id)).await["result"],
        json!({"allow": true})
    );

    let (id, request) = raise(
        &mut h,
        &mut fake,
        "request/question",
        json!({"question": "Which?", "options": ["A", "B"], "multiSelect": true, "allowFreeText": false}),
    )
    .await;
    assert_eq!(
        request.kind,
        RequestKind::Question {
            question: "Which?".into(),
            options: vec!["A".into(), "B".into()],
            multi_select: true,
            allow_free_text: false,
        }
    );
    request.responder.answer(Answer::Question("A, B".into()));
    assert_eq!(
        within(fake.response(&id)).await["result"],
        json!({"answer": "A, B"})
    );

    let (id, request) = raise(
        &mut h,
        &mut fake,
        "request/planApproval",
        json!({"plan": "1. do"}),
    )
    .await;
    assert_eq!(
        request.kind,
        RequestKind::Plan {
            plan: "1. do".into()
        }
    );
    request.responder.answer(Answer::Plan(false));
    assert_eq!(
        within(fake.response(&id)).await["result"],
        json!({"approve": false})
    );
}

#[tokio::test]
async fn declines_are_error_replies_never_empty_answers() {
    let mut h = harness();
    let mut fake = h.running().await;
    let cases: [(&str, Option<Answer>); 4] = [
        ("request/permission", Some(Answer::Decline)),
        ("request/permission", None),
        ("request/question", Some(Answer::Question("  ".into()))),
        ("request/permission", Some(Answer::Plan(true))),
    ];
    for (method, answer) in cases {
        let (id, request) = raise(&mut h, &mut fake, method, json!({})).await;
        match answer {
            Some(answer) => request.responder.answer(answer),
            None => drop(request),
        }
        let response = within(fake.response(&id)).await;
        assert_eq!(response["error"]["code"], -32800, "{response}");
        assert!(response.get("result").is_none());
    }
}

#[tokio::test]
async fn unknown_requests_are_refused() {
    let mut h = harness();
    let mut fake = h.running().await;
    let id = fake.send_request("request/somethingNew", json!({})).await;
    let response = within(fake.response(&id)).await;
    assert_eq!(response["error"]["code"], -32601);
    h.no_event().await;
}

#[tokio::test]
async fn interrupt_declines_open_requests() {
    let mut h = harness();
    let mut fake = h.running().await;
    let (request_id, request) = raise(
        &mut h,
        &mut fake,
        "request/permission",
        json!({"toolName": "x"}),
    )
    .await;
    let backend = Arc::clone(&h.backend);
    let interrupt = tokio::spawn(async move { backend.interrupt().await });
    let (id, _) = within(fake.expect("session/interrupt")).await;
    fake.respond(&id, json!({"ok": true})).await;
    within(interrupt).await.unwrap().unwrap();
    let response = within(fake.response(&request_id)).await;
    assert_eq!(response["error"]["code"], -32800);
    request.responder.answer(Answer::Permission(true));
}

#[tokio::test]
async fn coda_exiting_reports_exited_once_and_fails_the_prompt() {
    let mut h = harness();
    let mut fake = h.running().await;
    let prompt = h.prompt("work");
    within(fake.expect("session/prompt")).await;
    let (_, request) = raise(&mut h, &mut fake, "request/permission", json!({})).await;
    fake.exit("exit code: 3");
    let event = h.event().await;
    assert!(
        matches!(&event, BackendEvent::Exited { reason } if reason == "exit code: 3"),
        "{event:?}"
    );
    let error = within(prompt).await.unwrap().unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotRunning);
    request.responder.answer(Answer::Permission(true));
    h.no_event().await;
    let error = h.backend.prompt("more").await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotRunning);
}

#[tokio::test]
async fn a_closed_stream_reports_exited_once() {
    let mut h = harness();
    let fake = h.running().await;
    drop(fake);
    assert!(matches!(h.event().await, BackendEvent::Exited { .. }));
    h.no_event().await;
}

#[tokio::test]
async fn start_after_an_exit_respawns() {
    let mut h = harness();
    let fake = h.running().await;
    fake.exit("exit code: 1");
    assert!(matches!(h.event().await, BackendEvent::Exited { .. }));
    let (started, _fake, params) = h
        .spawn_start(options("/work/a", Some("s-1"), PermissionMode::Default))
        .await;
    started.unwrap();
    assert_eq!(params["sessionId"], "s-1");
    assert_eq!(h.launcher.spawns().len(), 2);
}

#[tokio::test]
async fn a_spawn_failure_is_not_running() {
    let mut h = harness();
    h.launcher.fail_spawns(1);
    let error = h
        .backend
        .start(options("/work/a", None, PermissionMode::Default))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotRunning);
    h.running().await;
}

#[tokio::test]
async fn a_folder_change_respawns_with_the_new_cwd() {
    let mut h = harness();
    let mut old = h.running().await;
    let backend = Arc::clone(&h.backend);
    let start = tokio::spawn(async move {
        backend
            .start(options("/work/b", None, PermissionMode::AcceptEdits))
            .await
    });
    let (id, _) = within(old.expect("shutdown")).await;
    old.respond(&id, json!({"ok": true})).await;
    let mut new = within(h.fakes.recv()).await.unwrap();
    within(new.handshake("s-2")).await;
    assert_eq!(within(start).await.unwrap().unwrap().session_id, "s-2");
    assert!(old.killed());
    assert_eq!(new.folder, PathBuf::from("/work/b"));
    assert_eq!(
        h.launcher.spawns(),
        vec![
            (PathBuf::from("/work/a"), PermissionMode::Default),
            (PathBuf::from("/work/b"), PermissionMode::AcceptEdits),
        ]
    );
    h.no_event().await;
}

#[tokio::test]
async fn a_different_mode_in_the_same_folder_is_set_on_the_running_process() {
    let mut h = harness();
    let mut fake = h.running().await;
    let backend = Arc::clone(&h.backend);
    let start = tokio::spawn(async move {
        backend
            .start(options("/work/a", None, PermissionMode::BypassPermissions))
            .await
    });
    within(fake.handshake("s-3")).await;
    let (id, params) = within(fake.expect("session/setPermissionMode")).await;
    assert_eq!(params, json!({"mode": "bypassPermissions"}));
    fake.respond(&id, json!({"ok": true, "applied": "bypassPermissions"}))
        .await;
    within(start).await.unwrap().unwrap();
    assert_eq!(h.launcher.spawns().len(), 1);
}

#[tokio::test]
async fn a_permission_mode_change_never_overlaps_a_start() {
    let mut h = harness();
    let mut fake = h.running().await;
    let command = h.command(BackendCommand::SetPermissionMode(
        PermissionMode::BypassPermissions,
    ));
    let (id, _) = within(fake.expect("session/setPermissionMode")).await;
    let backend = Arc::clone(&h.backend);
    let start = tokio::spawn(async move {
        backend
            .start(options("/work/a", None, PermissionMode::Default))
            .await
    });
    let early = tokio::time::timeout(Duration::from_millis(100), fake.request()).await;
    assert!(early.is_err(), "the start waits for the change: {early:?}");
    fake.respond(&id, json!({"ok": true, "applied": "bypassPermissions"}))
        .await;
    within(command).await.unwrap().unwrap();
    within(fake.handshake("s-2")).await;
    let (id, params) = within(fake.expect("session/setPermissionMode")).await;
    assert_eq!(params, json!({"mode": "default"}));
    fake.respond(&id, json!({"ok": true, "applied": "default"}))
        .await;
    within(start).await.unwrap().unwrap();
}

#[tokio::test]
async fn after_a_failed_mode_change_the_next_start_sets_the_mode_again() {
    let mut h = harness();
    let mut fake = h.running().await;
    let command = h.command(BackendCommand::SetPermissionMode(
        PermissionMode::BypassPermissions,
    ));
    let (id, _) = within(fake.expect("session/setPermissionMode")).await;
    fake.respond_error(&id, -32000, "lost").await;
    within(command).await.unwrap().unwrap_err();
    let backend = Arc::clone(&h.backend);
    let start = tokio::spawn(async move {
        backend
            .start(options("/work/a", None, PermissionMode::Default))
            .await
    });
    within(fake.handshake("s-2")).await;
    let (id, params) = within(fake.expect("session/setPermissionMode")).await;
    assert_eq!(params, json!({"mode": "default"}));
    fake.respond(&id, json!({"ok": true, "applied": "default"}))
        .await;
    within(start).await.unwrap().unwrap();
}

#[tokio::test]
async fn stop_shuts_coda_down_without_an_exit_event() {
    let mut h = harness();
    let mut fake = h.running().await;
    let backend = Arc::clone(&h.backend);
    let stop = tokio::spawn(async move { backend.stop().await });
    let (id, _) = within(fake.expect("shutdown")).await;
    fake.respond(&id, json!({"ok": true})).await;
    within(stop).await.unwrap();
    assert!(fake.killed());
    h.no_event().await;
    assert_eq!(
        h.backend.status().await.unwrap_err().kind,
        ErrorKind::NotRunning
    );
}

#[test]
fn the_serve_command_passes_arguments_as_a_list_and_drops_the_api_key() {
    let config = CodaConfig {
        exe: "coda".into(),
        args: vec!["--no-mcp".into()],
    };
    let command = serve_command(
        &config,
        Path::new("/work/my folder"),
        PermissionMode::BypassPermissions,
    );
    let command = command.as_std();
    assert_eq!(command.get_program(), "coda");
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_str().unwrap())
        .collect();
    assert_eq!(
        args,
        [
            "serve",
            "--no-mcp",
            "--cwd",
            "/work/my folder",
            "--permission-mode",
            "bypassPermissions"
        ]
    );
    assert!(
        command
            .get_envs()
            .any(|(name, value)| name == "CODA_SERVE_API_KEY" && value.is_none())
    );
}

#[test]
fn permission_mode_spellings() {
    for (mode, spelling) in [
        (PermissionMode::Default, "default"),
        (PermissionMode::AcceptEdits, "acceptEdits"),
        (PermissionMode::Plan, "plan"),
        (PermissionMode::BypassPermissions, "bypassPermissions"),
    ] {
        assert_eq!(cli_permission_mode(mode), spelling);
        assert_eq!(wire_permission_mode(mode), spelling);
    }
}

#[test]
fn tool_targets() {
    assert_eq!(
        tool_target(r#"{"command":"cargo\n test","path":"a.rs"}"#),
        "a.rs"
    );
    assert_eq!(tool_target(r#"{"file_path":"","cmd":"ls -la"}"#), "ls -la");
    assert_eq!(tool_target(r#"{"other":1}"#), "");
    assert_eq!(tool_target("not json\nat all"), "not json at all");
    assert_eq!(
        tool_target(&format!("{{\"url\":\"{}\"}}", "x".repeat(300)))
            .chars()
            .count(),
        200
    );
}

#[tokio::test]
async fn pending_messages_follow_the_pages() {
    let mut h = harness();
    let mut fake = h.running().await;
    let backend = Arc::clone(&h.backend);
    let pending =
        tokio::spawn(async move { backend.pending_messages(Some(5), Some("engine-1")).await });
    let (id, params) = within(fake.expect("session/pendingMessages")).await;
    assert_eq!(
        params,
        json!({"afterCursor": 5, "limit": 100, "engineInstanceId": "engine-1"})
    );
    let message = |cursor: u64| json!({"id": format!("m{cursor}"), "cursor": cursor, "label": "nightly", "text": format!("note {cursor}"), "source": "schedule"});
    fake.respond(
        &id,
        json!({"messages": [message(6), message(7)], "nextCursor": 7, "gap": true, "dropped": {"from": 1, "to": 5, "count": 5}, "truncated": true}),
    )
    .await;
    let (id, params) = within(fake.expect("session/pendingMessages")).await;
    assert_eq!(params["afterCursor"], 7);
    fake.respond(
        &id,
        json!({"messages": [message(8)], "nextCursor": 8, "gap": false, "truncated": false}),
    )
    .await;
    let pending = within(pending).await.unwrap().unwrap();
    assert_eq!(
        pending.messages,
        vec![
            (6, "nightly".to_owned(), "note 6".to_owned()),
            (7, "nightly".to_owned(), "note 7".to_owned()),
            (8, "nightly".to_owned(), "note 8".to_owned()),
        ]
    );
    assert!(pending.lost);
}

#[tokio::test]
async fn subagent_tools_carry_their_task_and_background_events_map() {
    let mut h = harness();
    let mut fake = h.running().await;
    fake.notify(
        "event/taskStarted",
        json!({"taskId": "task-1", "kind": "subagent", "description": "Review the parser", "mode": "background"}),
    )
    .await;
    fake.notify(
        "event/toolCall",
        json!({"toolName": "grep", "inputJson": "{\"pattern\":\"fn main\"}", "sourceId": "subagent:task-1"}),
    )
    .await;
    fake.notify(
        "event/toolResult",
        json!({"toolName": "grep", "isError": false, "status": "Cancelled"}),
    )
    .await;
    fake.notify(
        "event/taskStarted",
        json!({"taskId": "task-2", "kind": "shell", "description": "nested", "ownerTaskId": "task-1", "mode": "foreground"}),
    )
    .await;
    fake.notify(
        "event/taskCompleted",
        json!({"taskId": "task-2", "status": "completed", "ownerTaskId": "task-1"}),
    )
    .await;
    fake.notify(
        "event/agentMessage",
        json!({"id": "n1", "cursor": 4, "label": "nightly", "text": "done", "source": "schedule"}),
    )
    .await;
    fake.notify("event/error", json!({"message": "provider hiccup"}))
        .await;
    fake.notify(
        "event/taskCompleted",
        json!({"taskId": "task-1", "status": "completed", "report": "all good", "mode": "background", "kind": "subagent"}),
    )
    .await;

    let event = h.event().await;
    assert!(
        matches!(&event, BackendEvent::TaskStarted { id, description, background: true } if id == "task-1" && description == "Review the parser"),
        "{event:?}"
    );
    let BackendEvent::ToolStarted {
        id,
        tool,
        target,
        task,
    } = h.event().await
    else {
        panic!("a tool start");
    };
    assert_eq!((tool.as_str(), target.as_str()), ("grep", "fn main"));
    assert_eq!(task.as_deref(), Some("Review the parser"));
    let event = h.event().await;
    assert!(
        matches!(&event, BackendEvent::ToolFinished { id: finished, failed: true } if *finished == id),
        "{event:?}"
    );
    let event = h.event().await;
    assert!(
        matches!(&event, BackendEvent::AgentMessage { cursor: 4, label, text } if label == "nightly" && text == "done"),
        "{event:?}"
    );
    let event = h.event().await;
    assert!(
        matches!(&event, BackendEvent::Error(message) if message == "provider hiccup"),
        "{event:?}"
    );
    let event = h.event().await;
    assert!(
        matches!(
            &event,
            BackendEvent::TaskCompleted { id, description, status: TaskStatus::Completed, background: true, report: Some(report) }
                if id == "task-1" && description == "Review the parser" && report == "all good"
        ),
        "{event:?}"
    );
    h.no_event().await;
}

#[tokio::test]
async fn commands_call_coda_and_format_replies() {
    let mut h = harness();
    let mut fake = h.running().await;

    let fork = h.command(BackendCommand::Fork);
    let (id, _) = within(fake.expect("session/fork")).await;
    fake.respond(&id, json!({"ok": true, "newSessionId": "s-9"}))
        .await;
    let reply = within(fork).await.unwrap().unwrap();
    assert_eq!(reply.session.as_deref(), Some("s-9"));

    let models = h.command(BackendCommand::Models);
    let (id, params) = within(fake.expect("session/models")).await;
    assert_eq!(params, json!({"refresh": false}));
    fake.respond(
        &id,
        json!({"source": "live", "models": [{"id": "a"}, {"id": "b"}], "model": "b"}),
    )
    .await;
    let text = within(models).await.unwrap().unwrap().text;
    assert!(
        text.contains("- b (current)") && text.contains("- a\n"),
        "{text}"
    );

    let add = h.command(BackendCommand::ScheduleAdd {
        when: ScheduleWhen::Every("1h".into()),
        prompt: "check CI".into(),
    });
    let (id, params) = within(fake.expect("session/scheduleCreate")).await;
    assert_eq!(params, json!({"prompt": "check CI", "every": "1h"}));
    fake.respond(&id, json!({"id": "sch-1", "rule": "every 1h"}))
        .await;
    let text = within(add).await.unwrap().unwrap().text;
    assert!(text.starts_with("Scheduled sch-1: every 1h."), "{text}");
    assert!(text.ends_with(SCHEDULE_NOTE), "{text}");

    let sessions = h.command(BackendCommand::ListSessions);
    let (id, params) = within(fake.expect("session/listSessions")).await;
    assert_eq!(params, json!({"limit": 20}));
    fake.respond(
        &id,
        json!({"sessions": [{"sessionId": "s-1", "createdUtc": "2026-10-29", "messageCount": 4, "preview": "fix\nthe bug", "previewTruncated": false, "isCurrent": true}], "totalKnown": 1, "truncated": false}),
    )
    .await;
    let text = within(sessions).await.unwrap().unwrap().text;
    assert_eq!(text, "- s-1  2026-10-29  4 messages  fix the bug (current)");

    let compact = h.command(BackendCommand::Compact);
    let (id, _) = within(fake.expect("session/compact")).await;
    fake.respond(&id, json!({"ok": false, "error": "nothing to compact"}))
        .await;
    let error = within(compact).await.unwrap().unwrap_err();
    assert_eq!(
        (error.kind, error.message.as_str()),
        (ErrorKind::Refused, "nothing to compact")
    );
}

#[tokio::test]
async fn status_reads_model_tasks_and_version() {
    let mut h = harness();
    let mut fake = h.running().await;
    let backend = Arc::clone(&h.backend);
    let status = tokio::spawn(async move { backend.status().await });
    let (id, _) = within(fake.expect("session/models")).await;
    fake.respond_error(&id, -32603, "no provider").await;
    let (id, params) = within(fake.expect("session/listTasks")).await;
    assert_eq!(params, json!({"statusFilter": "active"}));
    fake.respond(
        &id,
        json!({"sessionId": "s-1", "tasks": [{"id": "task-1"}]}),
    )
    .await;
    assert_eq!(
        within(status).await.unwrap().unwrap(),
        BackendStatus {
            model: None,
            version: Some("coda (contract 2026-09-1)".into()),
            running_tasks: Some(1),
        }
    );
}
