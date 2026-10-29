//! [`CodaBackend`]: the [`Backend`] that drives `coda serve`.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, watch};

use super::rpc::{Incoming, RpcError, RpcPeer};
use crate::backend::{
    Answer, Backend, BackendCommand, BackendError, BackendEvent, BackendRequest, BackendStatus,
    Capability, CommandReply, ErrorKind, PendingMessages, PermissionMode, RequestKind, Responder,
    ScheduleWhen, StartOptions, Started, SteerOutcome, TaskStatus, TurnSource,
};
use crate::config::CodaConfig;
use crate::mask;

/// The JSON-RPC protocol version this backend speaks.
pub const PROTOCOL_VERSION: &str = "1";
/// How the bridge names itself in `initialize`.
pub const CLIENT_INFO: &str = "push-agent-bridge";
/// Environment variables never passed to Coda: in Coda's Rust engine
/// `CODA_SERVE_API_KEY` is an LLM credential, not the bridge's to set.
pub const REMOVED_ENV: [&str; 1] = ["CODA_SERVE_API_KEY"];
/// Appended to every schedule command's reply.
pub const SCHEDULE_NOTE: &str = "Schedules live in Coda's memory and are lost when Coda restarts.";

const SESSION_NOT_FOUND: i64 = -32002;
const METHOD_NOT_FOUND: i64 = -32601;
/// The canonical "declined" reply to a request from Coda.
const REQUEST_DECLINED: i64 = -32800;

/// Initialize waits for MCP servers to connect, which can take minutes.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(300);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Coda abandons a side question after 120 s.
const ASIDE_TIMEOUT: Duration = Duration::from_secs(150);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a closed stream waits for the process's exit status.
const EXIT_STATUS_WAIT: Duration = Duration::from_secs(1);

const SESSIONS_LIMIT: u64 = 20;
const PENDING_PAGE: u64 = 100;
const PENDING_MAX_PAGES: usize = 100;
/// Tool targets and list lines are single lines of at most this many characters.
const TARGET_CHARS: usize = 200;
const PREVIEW_CHARS: usize = 80;
/// The input fields that name what a tool works on, most telling first.
const TARGET_KEYS: [&str; 8] = [
    "path",
    "file_path",
    "command",
    "cmd",
    "query",
    "pattern",
    "url",
    "description",
];

/// A started `coda serve` process: its stdout, its stdin, its exit and a way to kill it.
pub struct Spawned {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    /// The exit status as text, once the process has exited.
    pub exited: oneshot::Receiver<String>,
    pub kill: Box<dyn FnOnce() + Send>,
}

/// Starts Coda processes; tests use [`super::testing::FakeLauncher`].
#[async_trait::async_trait]
pub trait Launcher: Send + Sync {
    async fn spawn(&self, folder: &Path, mode: PermissionMode) -> io::Result<Spawned>;
}

/// The `--permission-mode` spelling `coda serve` accepts.
pub fn cli_permission_mode(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
        PermissionMode::BypassPermissions => "bypassPermissions",
    }
}

/// The `session/setPermissionMode` spelling.
pub fn wire_permission_mode(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
        PermissionMode::BypassPermissions => "bypassPermissions",
    }
}

/// `<exe> serve <args…> --cwd <folder> --permission-mode <mode>`: arguments as a list,
/// never through a shell; stdin and stdout piped, stderr discarded, killed when dropped.
pub fn serve_command(config: &CodaConfig, folder: &Path, mode: PermissionMode) -> Command {
    let mut command = Command::new(&config.exe);
    command
        .arg("serve")
        .args(&config.args)
        .arg("--cwd")
        .arg(folder)
        .arg("--permission-mode")
        .arg(cli_permission_mode(mode))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for name in REMOVED_ENV {
        command.env_remove(name);
    }
    command
}

/// Starts real `coda serve` processes.
pub struct ProcessLauncher {
    config: CodaConfig,
}

impl ProcessLauncher {
    pub fn new(config: CodaConfig) -> Self {
        Self { config }
    }
}

#[async_trait::async_trait]
impl Launcher for ProcessLauncher {
    async fn spawn(&self, folder: &Path, mode: PermissionMode) -> io::Result<Spawned> {
        let mut child = serve_command(&self.config, folder, mode).spawn()?;
        let missing = || io::Error::other("coda serve has no stdio pipe");
        let writer = child.stdin.take().ok_or_else(missing)?;
        let reader = child.stdout.take().ok_or_else(missing)?;
        let (exited_sender, exited) = oneshot::channel();
        let (kill_sender, mut kill) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                Ok(()) = &mut kill => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let text = match status {
                Ok(status) => status.to_string(),
                Err(error) => format!("unknown exit status: {error}"),
            };
            let _ = exited_sender.send(text);
        });
        Ok(Spawned {
            reader: Box::new(reader),
            writer: Box::new(writer),
            exited,
            kill: Box::new(move || {
                let _ = kill_sender.send(());
            }),
        })
    }
}

/// What the event pump remembers about turns, tools and tasks.
#[derive(Default)]
struct Tracker {
    /// A prompt was sent and its turn has not started yet.
    prompt_pending: bool,
    /// `event/activity` and `event/turnEnded` arrive (negotiated state events).
    state_events: bool,
    /// The last turn seen.
    turn: Option<String>,
    turn_open: bool,
    /// Running tools, oldest first: `(id, tool name)`.
    tools: Vec<(String, String)>,
    /// Task descriptions by task id, for subagent tool labels.
    tasks: HashMap<String, String>,
    next_tool: u64,
}

impl Tracker {
    fn open_turn(&mut self, events: &mut Vec<BackendEvent>) {
        self.turn_open = true;
        self.tools.clear();
        let source = if std::mem::take(&mut self.prompt_pending) {
            TurnSource::Prompt
        } else {
            TurnSource::Engine
        };
        events.push(BackendEvent::TurnStarted { source });
    }

    fn end_turn(&mut self, interrupted: bool, events: &mut Vec<BackendEvent>) {
        self.turn_open = false;
        self.tools.clear();
        events.push(BackendEvent::TurnEnded { interrupted });
    }

    /// Without state events a turn starts with its first content.
    fn open_legacy_turn(&mut self, events: &mut Vec<BackendEvent>) {
        if !self.state_events && !self.turn_open {
            self.open_turn(events);
        }
    }
}

/// Which answer a request from Coda takes.
#[derive(Clone, Copy)]
enum Expect {
    Permission,
    Question,
    Plan,
}

/// The reply to send for `answer`; `None` declines.
fn reply_for(expect: Expect, answer: Answer) -> Option<Value> {
    match (expect, answer) {
        (Expect::Permission, Answer::Permission(allow)) => Some(json!({ "allow": allow })),
        (Expect::Question, Answer::Question(answer)) if !answer.trim().is_empty() => {
            Some(json!({ "answer": answer }))
        }
        (Expect::Plan, Answer::Plan(approve)) => Some(json!({ "approve": approve })),
        _ => None,
    }
}

/// One `coda serve` process and its connection.
struct Session {
    peer: RpcPeer,
    folder: PathBuf,
    mode: Mutex<PermissionMode>,
    version: Mutex<Option<String>>,
    /// Stopped on purpose: its end is not reported as [`BackendEvent::Exited`].
    retired: AtomicBool,
    kill: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    tracker: Mutex<Tracker>,
    /// Open requests from Coda, by local key: sending declines.
    requests: Mutex<HashMap<u64, oneshot::Sender<()>>>,
    next_request: AtomicU64,
    events: mpsc::UnboundedSender<BackendEvent>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Session {
    fn launch(
        spawned: Spawned,
        folder: PathBuf,
        mode: PermissionMode,
        events: mpsc::UnboundedSender<BackendEvent>,
    ) -> Arc<Self> {
        let (peer, incoming) = RpcPeer::new(spawned.reader, spawned.writer);
        let session = Arc::new(Self {
            peer: peer.clone(),
            folder,
            mode: Mutex::new(mode),
            version: Mutex::new(None),
            retired: AtomicBool::new(false),
            kill: Mutex::new(Some(spawned.kill)),
            tracker: Mutex::new(Tracker::default()),
            requests: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
            events,
        });
        let (status_sender, status) = watch::channel(None);
        let exited = spawned.exited;
        tokio::spawn(async move {
            let status = exited.await.unwrap_or_else(|_| "exited".to_owned());
            status_sender.send_replace(Some(status.clone()));
            peer.close(&status);
        });
        tokio::spawn(Arc::clone(&session).pump(incoming, status));
        session
    }

    fn is_running(&self) -> bool {
        !self.peer.is_closed()
    }

    fn emit(&self, events: Vec<BackendEvent>) {
        for event in events {
            let _ = self.events.send(event);
        }
    }

    async fn pump(
        self: Arc<Self>,
        mut incoming: mpsc::UnboundedReceiver<Incoming>,
        mut status: watch::Receiver<Option<String>>,
    ) {
        while let Some(message) = incoming.recv().await {
            match message {
                Incoming::Notification { method, params } => {
                    let events = self.on_notification(&method, &params);
                    self.emit(events);
                }
                Incoming::Request { id, method, params } => {
                    self.on_request(id, &method, &params);
                }
                Incoming::Closed { reason } => {
                    lock(&self.requests).clear();
                    if !self.retired.load(Ordering::SeqCst) {
                        let reason = exit_status(&mut status).await.unwrap_or(reason);
                        let _ = self.events.send(BackendEvent::Exited { reason });
                    }
                    return;
                }
            }
        }
    }

    fn on_notification(&self, method: &str, params: &Value) -> Vec<BackendEvent> {
        let mut events = Vec::new();
        let mut tracker = lock(&self.tracker);
        match method {
            "event/activity" => {
                tracker.state_events = true;
                let turn = text(params, "turnId");
                if tracker.turn.as_deref() != Some(turn.as_str()) {
                    tracker.turn = Some(turn);
                    // Compaction, fork and rewind hold the turn slot but are no reply.
                    if matches!(text(params, "phase").as_str(), "compacting" | "maintenance") {
                        tracker.turn_open = false;
                    } else {
                        tracker.open_turn(&mut events);
                    }
                }
            }
            "event/turnEnded" => {
                tracker.state_events = true;
                let turn = optional_text(params, "turnId");
                let same = turn.is_none() || turn == tracker.turn;
                if tracker.turn_open && same {
                    tracker.end_turn(flag(params, "interrupted"), &mut events);
                }
            }
            "event/turnComplete" => {
                if !tracker.state_events && tracker.turn_open {
                    tracker.end_turn(flag(params, "interrupted"), &mut events);
                }
            }
            "event/thinking" | "event/assistantText" => {
                let delta = text(params, "delta");
                if !delta.is_empty() {
                    tracker.open_legacy_turn(&mut events);
                    events.push(if method == "event/thinking" {
                        BackendEvent::ThinkingDelta(delta)
                    } else {
                        BackendEvent::TextDelta(delta)
                    });
                }
            }
            "event/toolCall" => {
                tracker.open_legacy_turn(&mut events);
                let tool = text(params, "toolName");
                let id = match optional_text(params, "callId") {
                    Some(id) if !tracker.tools.iter().any(|(running, _)| *running == id) => id,
                    _ => {
                        tracker.next_tool += 1;
                        format!("coda-tool-{}", tracker.next_tool)
                    }
                };
                let task = optional_text(params, "sourceId")
                    .and_then(|source| {
                        source
                            .strip_prefix("subagent:")
                            .map(|task| tracker.tasks.get(task).cloned())
                    })
                    .flatten();
                tracker.tools.push((id.clone(), tool.clone()));
                events.push(BackendEvent::ToolStarted {
                    id,
                    tool,
                    target: tool_target(&text(params, "inputJson")),
                    task,
                });
            }
            "event/toolResult" => {
                let tool = text(params, "toolName");
                let by_call = optional_text(params, "callId")
                    .and_then(|call| tracker.tools.iter().position(|(id, _)| *id == call));
                let index =
                    by_call.or_else(|| tracker.tools.iter().position(|(_, name)| *name == tool));
                if let Some(index) = index {
                    let (id, _) = tracker.tools.remove(index);
                    let status = optional_text(params, "status");
                    let failed = flag(params, "isError")
                        || matches!(status.as_deref(), Some("Failed" | "Cancelled"));
                    events.push(BackendEvent::ToolFinished { id, failed });
                }
            }
            "event/error" => events.push(BackendEvent::Error(text(params, "message"))),
            "event/agentMessage" => events.push(BackendEvent::AgentMessage {
                cursor: params.get("cursor").and_then(Value::as_u64).unwrap_or(0),
                label: text(params, "label"),
                text: text(params, "text"),
            }),
            "event/taskStarted" => {
                let id = text(params, "taskId");
                let description = text(params, "description");
                tracker.tasks.insert(id.clone(), description.clone());
                if optional_text(params, "ownerTaskId").is_none() {
                    events.push(BackendEvent::TaskStarted {
                        id,
                        description,
                        background: is_background(params),
                    });
                }
            }
            "event/taskCompleted" => {
                let id = text(params, "taskId");
                let known = tracker.tasks.remove(&id);
                if optional_text(params, "ownerTaskId").is_none() {
                    let description = optional_text(params, "description")
                        .or(known)
                        .unwrap_or_default();
                    let status = match text(params, "status").as_str() {
                        "completed" => TaskStatus::Completed,
                        "stopped" => TaskStatus::Stopped,
                        _ => TaskStatus::Failed,
                    };
                    events.push(BackendEvent::TaskCompleted {
                        id,
                        description,
                        status,
                        background: is_background(params),
                        report: optional_text(params, "report"),
                    });
                }
            }
            _ => {}
        }
        events
    }

    fn on_request(self: &Arc<Self>, id: Value, method: &str, params: &Value) {
        let (expect, kind) = match method {
            "request/permission" => (
                Expect::Permission,
                RequestKind::Permission {
                    tool: text(params, "toolName"),
                    preview: text(params, "inputPreview"),
                },
            ),
            "request/question" => (
                Expect::Question,
                RequestKind::Question {
                    question: text(params, "question"),
                    options: params
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|options| {
                            options
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default(),
                    multi_select: flag(params, "multiSelect"),
                    allow_free_text: flag(params, "allowFreeText"),
                },
            ),
            "request/planApproval" => (
                Expect::Plan,
                RequestKind::Plan {
                    plan: text(params, "plan"),
                },
            ),
            _ => {
                self.peer
                    .respond_error(id, METHOD_NOT_FOUND, "method not found");
                return;
            }
        };
        let key = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (decline, declined) = oneshot::channel();
        lock(&self.requests).insert(key, decline);
        let (responder, answer) = Responder::new();
        let _ = self
            .events
            .send(BackendEvent::Request(BackendRequest { kind, responder }));
        let session = Arc::clone(self);
        tokio::spawn(async move {
            let answer = tokio::select! {
                answer = answer => answer.ok(),
                _ = declined => None,
                () = session.peer.closed() => return,
            };
            lock(&session.requests).remove(&key);
            match answer.and_then(|answer| reply_for(expect, answer)) {
                Some(reply) => session.peer.respond(id, reply),
                None => session.peer.respond_error(id, REQUEST_DECLINED, "declined"),
            }
        });
    }

    /// Declines every open request from Coda.
    fn decline_open_requests(&self) {
        for (_, decline) in lock(&self.requests).drain() {
            let _ = decline.send(());
        }
    }

    async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, BackendError> {
        self.peer
            .call(method, params, timeout)
            .await
            .map_err(|error| backend_error(method, error))
    }

    async fn set_permission_mode(&self, mode: PermissionMode) -> Result<String, BackendError> {
        const METHOD: &str = "session/setPermissionMode";
        let wanted = wire_permission_mode(mode);
        let result = self
            .call(METHOD, json!({ "mode": wanted }), Some(CALL_TIMEOUT))
            .await?;
        let applied = optional_text(&result, "applied").unwrap_or_else(|| wanted.to_owned());
        if result.get("ok").and_then(Value::as_bool) == Some(false) || applied != wanted {
            return Err(refused(
                METHOD,
                format!("Coda kept the permission mode {applied}"),
            ));
        }
        *lock(&self.mode) = mode;
        Ok(applied)
    }

    /// Stops the process on purpose: `shutdown`, then kill.
    async fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
        if self.is_running() {
            let _ = self
                .peer
                .call("shutdown", json!({}), Some(SHUTDOWN_TIMEOUT))
                .await;
        }
        if let Some(kill) = lock(&self.kill).take() {
            kill();
        }
        self.peer.close("stopped");
    }
}

/// The process's exit status, if it arrives soon.
async fn exit_status(status: &mut watch::Receiver<Option<String>>) -> Option<String> {
    let waited = tokio::time::timeout(EXIT_STATUS_WAIT, status.wait_for(Option::is_some)).await;
    match waited {
        Ok(Ok(status)) => status.clone(),
        _ => None,
    }
}

fn text(params: &Value, key: &str) -> String {
    optional_text(params, key).unwrap_or_default()
}

fn optional_text(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn flag(params: &Value, key: &str) -> bool {
    params.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn is_background(params: &Value) -> bool {
    params.get("mode").and_then(Value::as_str) == Some("background")
}

/// Whitespace collapsed to single spaces, capped at `max` characters.
fn single_line(text: &str, max: usize) -> String {
    mask::cap(&text.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

/// What a tool works on, from its JSON input: the first present of [`TARGET_KEYS`];
/// input that is not JSON is used as it is.
fn tool_target(input: &str) -> String {
    let target = match serde_json::from_str::<Value>(input) {
        Ok(Value::Object(fields)) => TARGET_KEYS
            .iter()
            .find_map(|key| match fields.get(*key) {
                Some(Value::String(value)) if !value.trim().is_empty() => Some(value.clone()),
                Some(value @ (Value::Number(_) | Value::Bool(_))) => Some(value.to_string()),
                _ => None,
            })
            .unwrap_or_default(),
        Ok(_) => String::new(),
        Err(_) => input.to_owned(),
    };
    single_line(&target, TARGET_CHARS)
}

fn backend_error(method: &str, error: RpcError) -> BackendError {
    match error {
        RpcError::Remote { code, message } => {
            let kind = if code == SESSION_NOT_FOUND {
                ErrorKind::SessionNotFound
            } else if message.to_lowercase().contains("busy") {
                ErrorKind::Busy
            } else {
                ErrorKind::Refused
            };
            BackendError {
                method: method.to_owned(),
                code: code.to_string(),
                message,
                kind,
            }
        }
        RpcError::Closed => not_running(method, "exited"),
        RpcError::Timeout => BackendError {
            method: method.to_owned(),
            code: "timeout".to_owned(),
            message: "Coda did not answer in time".to_owned(),
            kind: ErrorKind::Other,
        },
    }
}

fn not_running(method: &str, code: &str) -> BackendError {
    BackendError {
        method: method.to_owned(),
        code: code.to_owned(),
        message: "Coda is not running".to_owned(),
        kind: ErrorKind::NotRunning,
    }
}

fn refused(method: &str, message: String) -> BackendError {
    BackendError {
        method: method.to_owned(),
        code: "refused".to_owned(),
        message,
        kind: ErrorKind::Refused,
    }
}

/// `serverInfo`, plus the contract version when Coda reports one.
fn version_text(result: &Value) -> Option<String> {
    let server = match result.get("serverInfo") {
        Some(Value::String(name)) => Some(name.clone()),
        Some(Value::Object(info)) => {
            let part = |key: &str| info.get(key).and_then(Value::as_str);
            match (part("name"), part("version")) {
                (Some(name), Some(version)) => Some(format!("{name} {version}")),
                (name, version) => name.or(version).map(str::to_owned),
            }
        }
        _ => None,
    };
    let contract = optional_text(result, "contractVersion");
    match (server, contract) {
        (Some(server), Some(contract)) => Some(format!("{server} (contract {contract})")),
        (Some(server), None) => Some(server),
        (None, Some(contract)) => Some(format!("contract {contract}")),
        (None, None) => None,
    }
}

fn is_refusal(result: &Value) -> bool {
    result.get("ok").and_then(Value::as_bool) == Some(false)
}

/// The reason Coda gave for an `ok: false` result.
fn refusal_reason(result: &Value) -> String {
    ["error", "reason", "note"]
        .iter()
        .find_map(|key| optional_text(result, key))
        .unwrap_or_else(|| "refused".to_owned())
}

fn items<'a>(result: &'a Value, key: &str) -> &'a [Value] {
    result
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn format_models(result: &Value) -> String {
    let current = optional_text(result, "model");
    let mut lines = Vec::new();
    if let Some(current) = &current {
        lines.push(format!("Current model: {current}"));
    }
    let models = items(result, "models");
    if models.is_empty() {
        lines.push("Coda reported no models.".to_owned());
    } else {
        lines.push("Models:".to_owned());
        for model in models {
            let id = text(model, "id");
            let mark = if current.as_deref() == Some(id.as_str()) {
                " (current)"
            } else {
                ""
            };
            lines.push(format!("- {id}{mark}"));
        }
    }
    lines.join("\n")
}

fn format_sessions(result: &Value) -> String {
    let sessions = items(result, "sessions");
    if sessions.is_empty() {
        return "No saved sessions in this folder.".to_owned();
    }
    sessions
        .iter()
        .map(|session| {
            let count = session
                .get("messageCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let current = if flag(session, "isCurrent") {
                " (current)"
            } else {
                ""
            };
            format!(
                "- {}  {}  {count} messages  {}{current}",
                text(session, "sessionId"),
                text(session, "createdUtc"),
                single_line(&text(session, "preview"), PREVIEW_CHARS),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_tasks(result: &Value) -> String {
    let tasks = items(result, "tasks");
    if tasks.is_empty() {
        return "No running tasks.".to_owned();
    }
    tasks
        .iter()
        .map(|task| {
            format!(
                "- {}  {}  {}  {}",
                text(task, "id"),
                text(task, "kind"),
                text(task, "mode"),
                single_line(&text(task, "description"), PREVIEW_CHARS),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_schedules(result: &Value) -> String {
    let schedules = items(result, "schedules");
    let list = if schedules.is_empty() {
        "No schedules.".to_owned()
    } else {
        schedules
            .iter()
            .map(|schedule| {
                let next = optional_text(schedule, "nextRunUtc")
                    .map(|next| format!("  next {next}"))
                    .unwrap_or_default();
                format!(
                    "- {}  {}  {}{next}  {}",
                    text(schedule, "id"),
                    text(schedule, "rule"),
                    text(schedule, "state"),
                    single_line(&text(schedule, "prompt"), PREVIEW_CHARS),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    with_schedule_note(list)
}

fn with_schedule_note(text: String) -> String {
    format!("{text}\n\n{SCHEDULE_NOTE}")
}

fn format_skills(result: &Value) -> String {
    let skills = items(result, "skills");
    if skills.is_empty() {
        return "No skills.".to_owned();
    }
    skills
        .iter()
        .map(|skill| {
            let disabled = if skill.get("enabled").and_then(Value::as_bool) == Some(false) {
                " (disabled)"
            } else {
                ""
            };
            match optional_text(skill, "description") {
                Some(description) => format!(
                    "- {}{disabled}: {}",
                    text(skill, "name"),
                    single_line(&description, PREVIEW_CHARS)
                ),
                None => format!("- {}{disabled}", text(skill, "name")),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn reply(text: String) -> CommandReply {
    CommandReply {
        text,
        session: None,
    }
}

/// The [`Backend`] for `coda serve`: one process per folder, restarted by the next
/// [`Backend::start`] after it exits.
pub struct CodaBackend {
    launcher: Arc<dyn Launcher>,
    events: mpsc::UnboundedSender<BackendEvent>,
    /// Serializes start and stop.
    lifecycle: tokio::sync::Mutex<()>,
    active: Mutex<Option<Arc<Session>>>,
}

impl CodaBackend {
    /// Runs `coda serve` as configured.
    pub fn new(config: CodaConfig, events: mpsc::UnboundedSender<BackendEvent>) -> Self {
        Self::with_launcher(Arc::new(ProcessLauncher::new(config)), events)
    }

    pub fn with_launcher(
        launcher: Arc<dyn Launcher>,
        events: mpsc::UnboundedSender<BackendEvent>,
    ) -> Self {
        Self {
            launcher,
            events,
            lifecycle: tokio::sync::Mutex::new(()),
            active: Mutex::new(None),
        }
    }

    /// The running session, for `method`.
    fn session(&self, method: &str) -> Result<Arc<Session>, BackendError> {
        lock(&self.active)
            .clone()
            .filter(|session| session.is_running())
            .ok_or_else(|| not_running(method, "not-running"))
    }

    async fn simple(
        session: &Session,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, BackendError> {
        let result = session.call(method, params, timeout).await?;
        if is_refusal(&result) {
            return Err(refused(method, refusal_reason(&result)));
        }
        Ok(result)
    }
}

#[async_trait::async_trait]
impl Backend for CodaBackend {
    fn name(&self) -> &str {
        "Coda"
    }

    fn capabilities(&self) -> BTreeSet<Capability> {
        BTreeSet::from([
            Capability::Steer,
            Capability::PlanApproval,
            Capability::Tasks,
            Capability::Sessions,
            Capability::Fork,
            Capability::Compact,
            Capability::Models,
            Capability::Effort,
            Capability::PermissionModes,
            Capability::Goal,
            Capability::AskAside,
            Capability::Schedules,
            Capability::Skills,
            Capability::AgentMessages,
        ])
    }

    async fn start(&self, options: StartOptions) -> Result<Started, BackendError> {
        const METHOD: &str = "initialize";
        let _lifecycle = self.lifecycle.lock().await;
        let current = lock(&self.active).clone();
        let reusable = current
            .as_ref()
            .filter(|session| session.is_running() && session.folder == options.folder)
            .cloned();
        let session = match reusable {
            Some(session) => session,
            None => {
                let old = lock(&self.active).take();
                if let Some(old) = old {
                    old.retire().await;
                }
                let spawned = self
                    .launcher
                    .spawn(&options.folder, options.permission_mode)
                    .await
                    .map_err(|error| BackendError {
                        method: "spawn".to_owned(),
                        code: "spawn".to_owned(),
                        message: format!("cannot start Coda: {error}"),
                        kind: ErrorKind::NotRunning,
                    })?;
                let session = Session::launch(
                    spawned,
                    options.folder.clone(),
                    options.permission_mode,
                    self.events.clone(),
                );
                *lock(&self.active) = Some(Arc::clone(&session));
                session
            }
        };
        let mut params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientInfo": CLIENT_INFO,
            "clientCapabilities": { "stateEvents": true },
        });
        if let Some(id) = &options.session {
            params["sessionId"] = json!(id);
        }
        let result = session
            .call(METHOD, params, Some(INITIALIZE_TIMEOUT))
            .await?;
        let session_id = optional_text(&result, "sessionId").ok_or_else(|| BackendError {
            method: METHOD.to_owned(),
            code: "protocol".to_owned(),
            message: "Coda sent no session id".to_owned(),
            kind: ErrorKind::Other,
        })?;
        let turn_events = result
            .pointer("/capabilities/state.turnEvents/supported")
            .and_then(Value::as_bool)
            == Some(true);
        if turn_events {
            lock(&session.tracker).state_events = true;
        }
        let version = version_text(&result);
        *lock(&session.version) = version.clone();
        if *lock(&session.mode) != options.permission_mode {
            session.set_permission_mode(options.permission_mode).await?;
        }
        Ok(Started {
            session_id,
            engine_instance: optional_text(&result, "engineInstanceId"),
            version,
        })
    }

    async fn stop(&self) {
        let _lifecycle = self.lifecycle.lock().await;
        let current = lock(&self.active).take();
        if let Some(session) = current {
            session.retire().await;
        }
    }

    async fn prompt(&self, text: &str) -> Result<(), BackendError> {
        const METHOD: &str = "session/prompt";
        let session = self.session(METHOD)?;
        lock(&session.tracker).prompt_pending = true;
        let result = session.call(METHOD, json!({ "text": text }), None).await;
        lock(&session.tracker).prompt_pending = false;
        let result = result?;
        if is_refusal(&result) && !flag(&result, "interrupted") {
            if let Some(error) = optional_text(&result, "error") {
                return Err(BackendError {
                    method: METHOD.to_owned(),
                    code: "turn".to_owned(),
                    message: error,
                    kind: ErrorKind::Other,
                });
            }
        }
        Ok(())
    }

    async fn steer(&self, text: &str) -> Result<SteerOutcome, BackendError> {
        const METHOD: &str = "session/steer";
        let session = self.session(METHOD)?;
        let result = session
            .call(METHOD, json!({ "text": text }), Some(CALL_TIMEOUT))
            .await?;
        if flag(&result, "ok") {
            return Ok(SteerOutcome::Accepted);
        }
        Ok(match optional_text(&result, "rejectedReason").as_deref() {
            Some("noActiveTurn") => SteerOutcome::NoActiveTurn,
            Some("turnEnding") => SteerOutcome::TurnEnding,
            Some(reason) => SteerOutcome::Rejected(reason.to_owned()),
            None => SteerOutcome::Rejected("rejected".to_owned()),
        })
    }

    async fn interrupt(&self) -> Result<(), BackendError> {
        const METHOD: &str = "session/interrupt";
        let session = self.session(METHOD)?;
        let result = session.call(METHOD, json!({}), Some(CALL_TIMEOUT)).await;
        session.decline_open_requests();
        result.map(|_| ())
    }

    async fn command(&self, command: BackendCommand) -> Result<CommandReply, BackendError> {
        let session = self.session("command")?;
        let session = session.as_ref();
        let timeout = Some(CALL_TIMEOUT);
        match command {
            BackendCommand::Compact => {
                let result = Self::simple(session, "session/compact", json!({}), None).await?;
                let count = |key: &str| result.get(key).and_then(Value::as_u64);
                Ok(reply(
                    match (count("messagesBefore"), count("messagesAfter")) {
                        (Some(before), Some(after)) => {
                            format!("Compacted the conversation: {before} → {after} messages.")
                        }
                        _ => "Compacted the conversation.".to_owned(),
                    },
                ))
            }
            BackendCommand::Models => {
                let params = json!({ "refresh": false });
                let result = Self::simple(session, "session/models", params, timeout).await?;
                Ok(reply(format_models(&result)))
            }
            BackendCommand::SetModel(model) => {
                let params = json!({ "model": model });
                let result = Self::simple(session, "session/setModel", params, timeout).await?;
                let model = optional_text(&result, "model").unwrap_or(model);
                Ok(reply(format!("Model: {model}.")))
            }
            BackendCommand::SetEffort(effort) => {
                let params = json!({ "effort": effort });
                let result = Self::simple(session, "session/setEffort", params, timeout).await?;
                let effort = optional_text(&result, "applied").unwrap_or(effort);
                Ok(reply(format!("Effort: {effort}.")))
            }
            BackendCommand::SetPermissionMode(mode) => {
                let applied = session.set_permission_mode(mode).await?;
                Ok(reply(format!("Permission mode: {applied}.")))
            }
            BackendCommand::SetGoal(goal) => {
                let cleared = goal.is_none();
                let params = json!({ "goal": goal });
                Self::simple(session, "session/setGoal", params, timeout).await?;
                Ok(reply(
                    if cleared {
                        "Goal cleared."
                    } else {
                        "Goal set."
                    }
                    .to_owned(),
                ))
            }
            BackendCommand::ListSessions => {
                let params = json!({ "limit": SESSIONS_LIMIT });
                let result = Self::simple(session, "session/listSessions", params, timeout).await?;
                Ok(reply(format_sessions(&result)))
            }
            BackendCommand::Fork => {
                const METHOD: &str = "session/fork";
                let result = Self::simple(session, METHOD, json!({}), None).await?;
                let id = optional_text(&result, "newSessionId")
                    .ok_or_else(|| refused(METHOD, "Coda sent no session id".to_owned()))?;
                Ok(CommandReply {
                    text: format!("Forked into session {id}."),
                    session: Some(id),
                })
            }
            BackendCommand::ListTasks => {
                let params = json!({ "statusFilter": "active" });
                let result = Self::simple(session, "session/listTasks", params, timeout).await?;
                Ok(reply(format_tasks(&result)))
            }
            BackendCommand::StopTask(id) => {
                let params = json!({ "taskId": id });
                let result = session.call("task/stop", params, timeout).await?;
                Ok(reply(if flag(&result, "ok") {
                    format!("Stopped task {id}.")
                } else {
                    let reason = optional_text(&result, "reason").unwrap_or_default();
                    format!("Task {id} was not stopped: {reason}.")
                }))
            }
            BackendCommand::AskAside(question) => {
                let params = json!({ "question": question });
                let result = session
                    .call("session/askAside", params, Some(ASIDE_TIMEOUT))
                    .await?;
                Ok(reply(match optional_text(&result, "answer") {
                    Some(answer) if flag(&result, "ok") => answer,
                    _ => format!("No answer: {}.", refusal_reason(&result)),
                }))
            }
            BackendCommand::ScheduleList => {
                let result =
                    Self::simple(session, "session/scheduleList", json!({}), timeout).await?;
                Ok(reply(format_schedules(&result)))
            }
            BackendCommand::ScheduleAdd { when, prompt } => {
                let mut params = json!({ "prompt": prompt });
                let (key, value) = match when {
                    ScheduleWhen::Every(value) => ("every", value),
                    ScheduleWhen::At(value) => ("at", value),
                    ScheduleWhen::Cron(value) => ("cron", value),
                };
                params[key] = json!(value);
                let result =
                    Self::simple(session, "session/scheduleCreate", params, timeout).await?;
                let id = text(&result, "id");
                let rule = optional_text(&result, "rule").unwrap_or(value);
                Ok(reply(with_schedule_note(format!(
                    "Scheduled {id}: {rule}."
                ))))
            }
            BackendCommand::ScheduleRemove(id) => {
                let params = json!({ "id": id });
                Self::simple(session, "session/scheduleDelete", params, timeout).await?;
                Ok(reply(with_schedule_note(format!("Removed schedule {id}."))))
            }
            BackendCommand::Skills => {
                let result = Self::simple(session, "skills/list", json!({}), timeout).await?;
                Ok(reply(format_skills(&result)))
            }
        }
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        let session = self.session("status")?;
        let timeout = Some(CALL_TIMEOUT);
        let model = session
            .call("session/models", json!({ "refresh": false }), timeout)
            .await
            .ok()
            .and_then(|result| optional_text(&result, "model"));
        let running_tasks = session
            .call(
                "session/listTasks",
                json!({ "statusFilter": "active" }),
                timeout,
            )
            .await
            .ok()
            .and_then(|result| result.get("tasks").and_then(Value::as_array).map(Vec::len));
        let version = lock(&session.version).clone();
        Ok(BackendStatus {
            model,
            version,
            running_tasks,
        })
    }

    async fn pending_messages(
        &self,
        after: Option<u64>,
        engine_instance: Option<&str>,
    ) -> Result<PendingMessages, BackendError> {
        const METHOD: &str = "session/pendingMessages";
        let session = self.session(METHOD)?;
        let mut cursor = after.unwrap_or(0);
        let mut pending = PendingMessages::default();
        for _ in 0..PENDING_MAX_PAGES {
            let mut params = json!({ "afterCursor": cursor, "limit": PENDING_PAGE });
            if let Some(instance) = engine_instance {
                params["engineInstanceId"] = json!(instance);
            }
            let result = session.call(METHOD, params, Some(CALL_TIMEOUT)).await?;
            for message in items(&result, "messages") {
                pending.messages.push((
                    message.get("cursor").and_then(Value::as_u64).unwrap_or(0),
                    text(message, "label"),
                    text(message, "text"),
                ));
            }
            pending.lost |= flag(&result, "gap");
            let next = result.get("nextCursor").and_then(Value::as_u64);
            match next {
                Some(next) if flag(&result, "truncated") && next > cursor => cursor = next,
                _ => break,
            }
        }
        Ok(pending)
    }
}

#[cfg(test)]
mod tests;
