//! The interface between the core and an agent backend (Coda first). The core calls a
//! [`Backend`]; the backend reports back with [`BackendEvent`]s through the
//! `mpsc::UnboundedSender<BackendEvent>` it was built with.
//!
//! Events, requests and answers carry message text, so their `Debug` output shows only
//! what kind they are and how long their text is.

use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

pub use crate::commands::ScheduleWhen;

/// How a backend writes the text of its turns and background messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TextFormat {
    /// Plain text, shown as it is (`agent.reply@1`): `2 * 3 * 4` stays as typed.
    #[default]
    Plain,
    /// Markdown, shown formatted (`agent.reply@2`, the safe subset the server checks).
    Markdown,
}

/// What a backend can do beyond prompts; the core offers only what the backend has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Capability {
    Steer,
    PlanApproval,
    Tasks,
    Sessions,
    Fork,
    Compact,
    Models,
    Effort,
    PermissionModes,
    Goal,
    AskAside,
    Schedules,
    Skills,
    AgentMessages,
}

/// How the agent asks before it uses a tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    #[default]
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    #[serde(rename = "plan")]
    Plan,
    /// Every tool runs without asking.
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    /// The mode named by [`Self::as_str`].
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "default" => Some(Self::Default),
            "acceptEdits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "bypassPermissions" => Some(Self::BypassPermissions),
            _ => None,
        }
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Where and how to start the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartOptions {
    pub folder: PathBuf,
    /// The session to resume; `None` starts a new one.
    pub session: Option<String>,
    pub permission_mode: PermissionMode,
}

/// A started (or resumed) session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub session_id: String,
    /// Identifies the agent process's engine: background messages can be fetched again
    /// only from the same instance.
    pub engine_instance: Option<String>,
    pub version: Option<String>,
}

/// What kind of failure a [`BackendError`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// A turn is running.
    Busy,
    /// The session is unknown in this folder.
    SessionNotFound,
    /// The agent is not running.
    NotRunning,
    /// The agent refused the call, for example while busy or for bad arguments.
    Refused,
    Other,
}

/// A failed call into the backend.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{method} failed ({code}): {message}")]
pub struct BackendError {
    /// The backend's method, for example `session/prompt`.
    pub method: String,
    /// The backend's error code, such as `-32603`, or `exited`.
    pub code: String,
    pub message: String,
    pub kind: ErrorKind,
}

/// How a steer went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerOutcome {
    Accepted,
    /// No turn is running: send the text as a prompt.
    NoActiveTurn,
    /// The turn is ending: send the text as a prompt once it has ended.
    TurnEnding,
    /// Refused for another reason, in the backend's words.
    Rejected(String),
}

/// What started a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnSource {
    /// The owner's prompt.
    Prompt,
    /// The engine itself, for example after a background task finished.
    Engine,
}

/// What the agent asks the owner.
#[derive(Clone, PartialEq, Eq)]
pub enum RequestKind {
    /// May the agent use `tool`? `preview` shows what it would do.
    Permission { tool: String, preview: String },
    Question {
        question: String,
        options: Vec<String>,
        multi_select: bool,
        allow_free_text: bool,
    },
    /// Approve this plan?
    Plan { plan: String },
}

/// The owner's answer to a [`RequestKind`].
#[derive(Clone, PartialEq, Eq)]
pub enum Answer {
    Permission(bool),
    /// The chosen label(s) or the owner's text; never empty.
    Question(String),
    Plan(bool),
    /// No answer: the request is declined.
    Decline,
}

/// The way back to the backend for one request. Dropping it without answering declines.
pub struct Responder(oneshot::Sender<Answer>);

impl Responder {
    /// A responder and the receiver its answer arrives on (an error there means declined).
    pub fn new() -> (Self, oneshot::Receiver<Answer>) {
        let (sender, receiver) = oneshot::channel();
        (Self(sender), receiver)
    }

    /// Sends the answer; a backend that stopped waiting is no error.
    pub fn answer(self, answer: Answer) {
        let _ = self.0.send(answer);
    }
}

impl fmt::Debug for Responder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Responder")
    }
}

/// A request from the agent together with the way to answer it.
#[derive(Debug)]
pub struct BackendRequest {
    pub kind: RequestKind,
    pub responder: Responder,
}

/// How a task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Completed,
    Failed,
    Stopped,
}

/// What a backend reports.
pub enum BackendEvent {
    TurnStarted {
        source: TurnSource,
    },
    ThinkingDelta(String),
    TextDelta(String),
    ToolStarted {
        id: String,
        tool: String,
        target: String,
        /// The subagent's task description when the tool runs in a subagent.
        task: Option<String>,
    },
    ToolFinished {
        id: String,
        failed: bool,
    },
    Request(BackendRequest),
    /// A message the agent sent on its own, outside a turn's reply.
    AgentMessage {
        cursor: u64,
        label: String,
        text: String,
    },
    TaskStarted {
        id: String,
        description: String,
        background: bool,
    },
    TaskCompleted {
        id: String,
        description: String,
        status: TaskStatus,
        background: bool,
        report: Option<String>,
    },
    Error(String),
    TurnEnded {
        interrupted: bool,
    },
    /// The backend process stopped or its stream broke; open requests are already declined.
    Exited {
        reason: String,
    },
}

/// A backend command, from the owner's `/…` commands.
#[derive(Clone, PartialEq, Eq)]
pub enum BackendCommand {
    Compact,
    Models,
    SetModel(String),
    SetEffort(String),
    SetPermissionMode(PermissionMode),
    /// `None` clears the goal.
    SetGoal(Option<String>),
    ListSessions,
    Fork,
    ListTasks,
    StopTask(String),
    AskAside(String),
    ScheduleList,
    ScheduleAdd {
        when: ScheduleWhen,
        prompt: String,
    },
    ScheduleRemove(String),
    Skills,
}

/// A backend command's result, as plain text the core masks before it is sent.
#[derive(Clone, PartialEq, Eq)]
pub struct CommandReply {
    pub text: String,
    /// A new session id (fork).
    pub session: Option<String>,
}

/// What `/status` shows about the backend; `None` when the backend cannot tell.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BackendStatus {
    pub model: Option<String>,
    pub version: Option<String>,
    pub running_tasks: Option<usize>,
}

/// Background messages fetched again.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct PendingMessages {
    /// `(cursor, label, text)`, oldest first.
    pub messages: Vec<(u64, String, String)>,
    /// Some messages after the cursor are gone.
    pub lost: bool,
}

/// An agent backend the core drives.
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// "Coda": used in replies such as "(Coda stopped; restarting)".
    fn name(&self) -> &str;
    fn capabilities(&self) -> BTreeSet<Capability>;
    /// How the agent writes its text; plain unless the backend says Markdown.
    fn text_format(&self) -> TextFormat {
        TextFormat::Plain
    }
    /// Starts the agent in `options.folder` (restarting its process when the folder
    /// changed or it is not running) and opens `options.session`, or a new session when
    /// `None`. [`ErrorKind::SessionNotFound`] when the session is unknown in that folder.
    async fn start(&self, options: StartOptions) -> Result<Started, BackendError>;
    async fn stop(&self);
    /// Runs one prompt; resolves when its turn ends. [`ErrorKind::Busy`] when a turn is
    /// running.
    async fn prompt(&self, text: &str) -> Result<(), BackendError>;
    async fn steer(&self, text: &str) -> Result<SteerOutcome, BackendError>;
    async fn interrupt(&self) -> Result<(), BackendError>;
    async fn command(&self, command: BackendCommand) -> Result<CommandReply, BackendError>;
    async fn status(&self) -> Result<BackendStatus, BackendError>;
    /// Background messages after `after` (`None`: from the start) of `engine_instance`.
    async fn pending_messages(
        &self,
        after: Option<u64>,
        engine_instance: Option<&str>,
    ) -> Result<PendingMessages, BackendError>;
}

/// `<n chars>`: how much text there is, never the text.
struct Length<'a>(&'a str);

impl fmt::Debug for Length<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "<{} chars>", self.0.chars().count())
    }
}

impl fmt::Debug for RequestKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Permission { tool, preview } => formatter
                .debug_struct("Permission")
                .field("tool", tool)
                .field("preview", &Length(preview))
                .finish(),
            Self::Question {
                question,
                options,
                multi_select,
                allow_free_text,
            } => formatter
                .debug_struct("Question")
                .field("question", &Length(question))
                .field("options", &options.len())
                .field("multi_select", multi_select)
                .field("allow_free_text", allow_free_text)
                .finish(),
            Self::Plan { plan } => formatter
                .debug_struct("Plan")
                .field("plan", &Length(plan))
                .finish(),
        }
    }
}

impl fmt::Debug for Answer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Permission(allow) => write!(formatter, "Permission({allow})"),
            Self::Question(answer) => write!(formatter, "Question({:?})", Length(answer)),
            Self::Plan(approve) => write!(formatter, "Plan({approve})"),
            Self::Decline => formatter.write_str("Decline"),
        }
    }
}

impl fmt::Debug for BackendEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TurnStarted { source } => write!(formatter, "TurnStarted({source:?})"),
            Self::ThinkingDelta(delta) => write!(formatter, "ThinkingDelta({:?})", Length(delta)),
            Self::TextDelta(delta) => write!(formatter, "TextDelta({:?})", Length(delta)),
            Self::ToolStarted { id, tool, task, .. } => formatter
                .debug_struct("ToolStarted")
                .field("id", id)
                .field("tool", tool)
                .field("subagent", &task.is_some())
                .finish_non_exhaustive(),
            Self::ToolFinished { id, failed } => formatter
                .debug_struct("ToolFinished")
                .field("id", id)
                .field("failed", failed)
                .finish(),
            Self::Request(request) => write!(formatter, "Request({:?})", request.kind),
            Self::AgentMessage { cursor, text, .. } => formatter
                .debug_struct("AgentMessage")
                .field("cursor", cursor)
                .field("text", &Length(text))
                .finish_non_exhaustive(),
            Self::TaskStarted { id, background, .. } => formatter
                .debug_struct("TaskStarted")
                .field("id", id)
                .field("background", background)
                .finish_non_exhaustive(),
            Self::TaskCompleted {
                id,
                status,
                background,
                ..
            } => formatter
                .debug_struct("TaskCompleted")
                .field("id", id)
                .field("status", status)
                .field("background", background)
                .finish_non_exhaustive(),
            Self::Error(message) => write!(formatter, "Error({:?})", Length(message)),
            Self::TurnEnded { interrupted } => write!(formatter, "TurnEnded({interrupted})"),
            Self::Exited { reason } => write!(formatter, "Exited({:?})", Length(reason)),
        }
    }
}

impl fmt::Debug for BackendCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SetModel(model) => write!(formatter, "SetModel({model:?})"),
            Self::SetEffort(effort) => write!(formatter, "SetEffort({effort:?})"),
            Self::SetPermissionMode(mode) => write!(formatter, "SetPermissionMode({mode})"),
            Self::SetGoal(goal) => write!(formatter, "SetGoal(set: {})", goal.is_some()),
            Self::StopTask(id) => write!(formatter, "StopTask({id:?})"),
            Self::AskAside(question) => write!(formatter, "AskAside({:?})", Length(question)),
            Self::ScheduleAdd { when, .. } => formatter
                .debug_struct("ScheduleAdd")
                .field("when", when)
                .finish_non_exhaustive(),
            Self::ScheduleRemove(id) => write!(formatter, "ScheduleRemove({id:?})"),
            Self::Compact => formatter.write_str("Compact"),
            Self::Models => formatter.write_str("Models"),
            Self::ListSessions => formatter.write_str("ListSessions"),
            Self::Fork => formatter.write_str("Fork"),
            Self::ListTasks => formatter.write_str("ListTasks"),
            Self::ScheduleList => formatter.write_str("ScheduleList"),
            Self::Skills => formatter.write_str("Skills"),
        }
    }
}

impl fmt::Debug for CommandReply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandReply")
            .field("text", &Length(&self.text))
            .field("session", &self.session)
            .finish()
    }
}

impl fmt::Debug for PendingMessages {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingMessages")
            .field("messages", &self.messages.len())
            .field("lost", &self.lost)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_modes_round_trip_by_name() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::BypassPermissions,
        ] {
            assert_eq!(PermissionMode::parse(mode.as_str()), Some(mode));
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{}\"", mode.as_str()));
            assert_eq!(serde_json::from_str::<PermissionMode>(&json).unwrap(), mode);
        }
        assert_eq!(PermissionMode::parse("bypass"), None);
    }

    #[tokio::test]
    async fn a_responder_delivers_its_answer() {
        let (responder, answer) = Responder::new();
        responder.answer(Answer::Permission(true));
        assert_eq!(answer.await, Ok(Answer::Permission(true)));
    }

    #[tokio::test]
    async fn a_dropped_responder_declines() {
        let (responder, answer) = Responder::new();
        drop(responder);
        assert!(answer.await.is_err());
    }

    #[test]
    fn debug_output_hides_message_text() {
        let secret = "hunter2-secret-words";
        let (responder, _answer) = Responder::new();
        let shown = [
            format!("{:?}", BackendEvent::TextDelta(secret.into())),
            format!("{:?}", BackendEvent::ThinkingDelta(secret.into())),
            format!(
                "{:?}",
                BackendEvent::ToolStarted {
                    id: "1".into(),
                    tool: "Bash".into(),
                    target: secret.into(),
                    task: Some(secret.into()),
                }
            ),
            format!(
                "{:?}",
                BackendEvent::Request(BackendRequest {
                    kind: RequestKind::Permission {
                        tool: "Bash".into(),
                        preview: secret.into(),
                    },
                    responder,
                })
            ),
            format!(
                "{:?}",
                RequestKind::Question {
                    question: secret.into(),
                    options: vec![secret.into()],
                    multi_select: false,
                    allow_free_text: true,
                }
            ),
            format!(
                "{:?}",
                RequestKind::Plan {
                    plan: secret.into()
                }
            ),
            format!("{:?}", Answer::Question(secret.into())),
            format!(
                "{:?}",
                BackendEvent::AgentMessage {
                    cursor: 1,
                    label: secret.into(),
                    text: secret.into(),
                }
            ),
            format!(
                "{:?}",
                BackendEvent::TaskCompleted {
                    id: "t".into(),
                    description: secret.into(),
                    status: TaskStatus::Completed,
                    background: true,
                    report: Some(secret.into()),
                }
            ),
            format!("{:?}", BackendCommand::AskAside(secret.into())),
            format!("{:?}", BackendCommand::SetGoal(Some(secret.into()))),
            format!(
                "{:?}",
                BackendCommand::ScheduleAdd {
                    when: ScheduleWhen::Every("1h".into()),
                    prompt: secret.into(),
                }
            ),
            format!(
                "{:?}",
                CommandReply {
                    text: secret.into(),
                    session: None,
                }
            ),
            format!(
                "{:?}",
                PendingMessages {
                    messages: vec![(1, secret.into(), secret.into())],
                    lost: false,
                }
            ),
        ];
        for shown in shown {
            assert!(!shown.contains("hunter2"), "{shown}");
        }
    }
}
