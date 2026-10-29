//! A scripted [`Backend`] for tests: records every call, emits the events a test asks
//! for and returns the results it queued. It proves the core runs without Coda.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant;

use crate::backend::{
    Answer, Backend, BackendCommand, BackendError, BackendEvent, BackendRequest, BackendStatus,
    Capability, CommandReply, ErrorKind, PendingMessages, RequestKind, Responder, StartOptions,
    Started, SteerOutcome, TextFormat, TurnSource,
};
use crate::messenger::wait;

/// Every capability a backend can have.
pub const ALL_CAPABILITIES: [Capability; 14] = [
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
];

struct Script {
    calls: Vec<(Instant, String)>,
    capabilities: BTreeSet<Capability>,
    starts: VecDeque<Result<Started, BackendError>>,
    prompts: VecDeque<Result<(), BackendError>>,
    steers: VecDeque<Result<SteerOutcome, BackendError>>,
    commands: VecDeque<Result<CommandReply, BackendError>>,
    /// The next commands wait for these before they return.
    command_holds: VecDeque<oneshot::Receiver<()>>,
    status: BackendStatus,
    pending: PendingMessages,
    engine_instance: Option<String>,
    sessions: u64,
    text_format: TextFormat,
}

/// A [`Backend`] a test scripts.
pub struct TestBackend {
    name: String,
    events: mpsc::UnboundedSender<BackendEvent>,
    script: Mutex<Script>,
    changed: Notify,
}

impl TestBackend {
    /// A backend named `name` with every capability, reporting through `events`.
    pub fn new(name: &str, events: mpsc::UnboundedSender<BackendEvent>) -> Self {
        Self {
            name: name.to_owned(),
            events,
            script: Mutex::new(Script {
                calls: Vec::new(),
                capabilities: ALL_CAPABILITIES.into_iter().collect(),
                starts: VecDeque::new(),
                prompts: VecDeque::new(),
                steers: VecDeque::new(),
                commands: VecDeque::new(),
                command_holds: VecDeque::new(),
                status: BackendStatus::default(),
                pending: PendingMessages::default(),
                engine_instance: Some("engine-1".to_owned()),
                sessions: 0,
                text_format: TextFormat::Plain,
            }),
            changed: Notify::new(),
        }
    }

    /// A backend named "Test" and the receiver of its events.
    pub fn channel() -> (Arc<Self>, mpsc::UnboundedReceiver<BackendEvent>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Arc::new(Self::new("Test", sender)), receiver)
    }

    pub fn set_capabilities(&self, capabilities: impl IntoIterator<Item = Capability>) {
        self.lock().capabilities = capabilities.into_iter().collect();
    }

    /// How the backend says it writes (plain unless set); the core reads it once.
    pub fn set_text_format(&self, format: TextFormat) {
        self.lock().text_format = format;
    }

    /// The engine instance the next starts report.
    pub fn set_engine_instance(&self, instance: Option<&str>) {
        self.lock().engine_instance = instance.map(str::to_owned);
    }

    /// The next start returns `result` (later ones succeed again).
    pub fn next_start(&self, result: Result<Started, BackendError>) {
        self.lock().starts.push_back(result);
    }

    /// The next start fails with `kind`.
    pub fn fail_next_start(&self, kind: ErrorKind) {
        self.next_start(Err(error("initialize", kind)));
    }

    pub fn next_prompt(&self, result: Result<(), BackendError>) {
        self.lock().prompts.push_back(result);
    }

    pub fn next_steer(&self, result: Result<SteerOutcome, BackendError>) {
        self.lock().steers.push_back(result);
    }

    pub fn next_command(&self, result: Result<CommandReply, BackendError>) {
        self.lock().commands.push_back(result);
    }

    /// The next command (recorded at once) returns only once the sender is used or
    /// dropped; its result is taken then.
    pub fn hold_next_command(&self) -> oneshot::Sender<()> {
        let (release, hold) = oneshot::channel();
        self.lock().command_holds.push_back(hold);
        release
    }

    pub fn set_status(&self, status: BackendStatus) {
        self.lock().status = status;
    }

    /// What the next `pending_messages` returns.
    pub fn set_pending(&self, pending: PendingMessages) {
        self.lock().pending = pending;
    }

    /// Reports `event` to the core.
    pub fn emit(&self, event: BackendEvent) {
        let _ = self.events.send(event);
    }

    pub fn turn_started(&self, source: TurnSource) {
        self.emit(BackendEvent::TurnStarted { source });
    }

    pub fn text(&self, delta: &str) {
        self.emit(BackendEvent::TextDelta(delta.to_owned()));
    }

    pub fn turn_ended(&self, interrupted: bool) {
        self.emit(BackendEvent::TurnEnded { interrupted });
    }

    /// Asks the owner `kind`; the receiver gets the answer (an error: declined).
    pub fn request(&self, kind: RequestKind) -> oneshot::Receiver<Answer> {
        let (responder, answer) = Responder::new();
        self.emit(BackendEvent::Request(BackendRequest { kind, responder }));
        answer
    }

    /// Every call so far, such as `"start folder=… session=None mode=default"`,
    /// `"prompt hi"`, `"steer x"`.
    pub fn calls(&self) -> Vec<String> {
        self.lock()
            .calls
            .iter()
            .map(|(_, call)| call.clone())
            .collect()
    }

    /// Every call with the (tokio) time it was made.
    pub fn timed_calls(&self) -> Vec<(Instant, String)> {
        self.lock().calls.clone()
    }

    /// Waits until a call starting with `prefix` was made `count` times; panics after 60 s.
    pub async fn wait_for(&self, prefix: &str, count: usize) {
        let what = format!("{count} × {prefix:?}");
        wait(&self.changed, &what, || {
            self.calls()
                .iter()
                .filter(|call| call.starts_with(prefix))
                .count()
                >= count
        })
        .await;
    }

    fn record(&self, call: String) {
        self.lock().calls.push((Instant::now(), call));
        self.changed.notify_waiters();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A [`BackendError`] of `kind` from `method`.
pub fn error(method: &str, kind: ErrorKind) -> BackendError {
    let (code, message) = match kind {
        ErrorKind::Busy => ("-32603", "busy"),
        ErrorKind::SessionNotFound => ("-32002", "session not found"),
        ErrorKind::NotRunning => ("exited", "not running"),
        ErrorKind::Refused => ("refused", "refused"),
        ErrorKind::Other => ("-32000", "failed"),
    };
    BackendError {
        method: method.to_owned(),
        code: code.to_owned(),
        message: message.to_owned(),
        kind,
    }
}

#[async_trait::async_trait]
impl Backend for TestBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> BTreeSet<Capability> {
        self.lock().capabilities.clone()
    }

    fn text_format(&self) -> TextFormat {
        self.lock().text_format
    }

    async fn start(&self, options: StartOptions) -> Result<Started, BackendError> {
        self.record(format!(
            "start folder={} session={} mode={}",
            options.folder.display(),
            options.session.as_deref().unwrap_or("None"),
            options.permission_mode,
        ));
        let mut script = self.lock();
        if let Some(result) = script.starts.pop_front() {
            return result;
        }
        let session_id = match options.session {
            Some(id) => id,
            None => {
                script.sessions += 1;
                format!("session-{}", script.sessions)
            }
        };
        Ok(Started {
            session_id,
            engine_instance: script.engine_instance.clone(),
            version: Some("1.0".to_owned()),
        })
    }

    async fn stop(&self) {
        self.record("stop".to_owned());
    }

    async fn prompt(&self, text: &str) -> Result<(), BackendError> {
        self.record(format!("prompt {text}"));
        self.lock().prompts.pop_front().unwrap_or(Ok(()))
    }

    async fn steer(&self, text: &str) -> Result<SteerOutcome, BackendError> {
        self.record(format!("steer {text}"));
        self.lock()
            .steers
            .pop_front()
            .unwrap_or(Ok(SteerOutcome::Accepted))
    }

    async fn interrupt(&self) -> Result<(), BackendError> {
        self.record("interrupt".to_owned());
        Ok(())
    }

    async fn command(&self, command: BackendCommand) -> Result<CommandReply, BackendError> {
        self.record(format!("command {command:?}"));
        let hold = self.lock().command_holds.pop_front();
        if let Some(hold) = hold {
            let _ = hold.await;
        }
        self.lock().commands.pop_front().unwrap_or_else(|| {
            Ok(CommandReply {
                text: "ok".to_owned(),
                session: None,
            })
        })
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        self.record("status".to_owned());
        Ok(self.lock().status.clone())
    }

    async fn pending_messages(
        &self,
        after: Option<u64>,
        engine_instance: Option<&str>,
    ) -> Result<PendingMessages, BackendError> {
        self.record(format!(
            "pending after={} engine={}",
            after.map_or("None".to_owned(), |cursor| cursor.to_string()),
            engine_instance.unwrap_or("None"),
        ));
        Ok(std::mem::take(&mut self.lock().pending))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::backend::PermissionMode;

    fn options(session: Option<&str>) -> StartOptions {
        StartOptions {
            folder: PathBuf::from("work"),
            session: session.map(str::to_owned),
            permission_mode: PermissionMode::Default,
        }
    }

    #[tokio::test]
    async fn starts_record_and_follow_the_script() {
        let (backend, _events) = TestBackend::channel();
        let started = backend.start(options(None)).await.unwrap();
        assert_eq!(started.session_id, "session-1");
        assert_eq!(
            backend.start(options(Some("s"))).await.unwrap().session_id,
            "s"
        );
        backend.fail_next_start(ErrorKind::SessionNotFound);
        let error = backend.start(options(Some("x"))).await.unwrap_err();
        assert_eq!(error.kind, ErrorKind::SessionNotFound);
        assert_eq!(
            backend.calls(),
            [
                "start folder=work session=None mode=default",
                "start folder=work session=s mode=default",
                "start folder=work session=x mode=default",
            ]
        );
    }

    #[tokio::test]
    async fn requests_carry_their_answers_back() {
        let (backend, mut events) = TestBackend::channel();
        let answer = backend.request(RequestKind::Plan { plan: "p".into() });
        let Some(BackendEvent::Request(request)) = events.recv().await else {
            panic!("a request");
        };
        request.responder.answer(Answer::Plan(true));
        assert_eq!(answer.await, Ok(Answer::Plan(true)));
    }

    #[tokio::test]
    async fn scripted_results_come_once() {
        let (backend, _events) = TestBackend::channel();
        backend.next_prompt(Err(error("session/prompt", ErrorKind::Busy)));
        backend.next_steer(Ok(SteerOutcome::TurnEnding));
        assert_eq!(backend.prompt("a").await.unwrap_err().kind, ErrorKind::Busy);
        assert_eq!(backend.prompt("b").await, Ok(()));
        assert_eq!(backend.steer("c").await, Ok(SteerOutcome::TurnEnding));
        assert_eq!(backend.steer("d").await, Ok(SteerOutcome::Accepted));
        backend.wait_for("steer", 2).await;
    }
}
