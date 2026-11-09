//! The echo backend: a deterministic test agent with no engine behind it. It echoes the
//! owner's and accepted contacts' texts (each in their own conversation), asks a question on `ask` and a multi-select on `pick`, and reports no
//! capabilities. Operators run it under the bridge for contract checks and manual tests:
//! sign-in, cards, activity, the tray and voice calls work as for Coda.
//!
//! | Text | Turn |
//! |---|---|
//! | `ask` | a question (Red, Blue; free text allowed), then "you chose: <answer>" |
//! | `pick` | a multi-select (Red, Blue, Green), then "you chose: <labels>" |
//! | an action command | the bridge action's result, such as "done: sent to @bob" |
//! | anything else | "echo: <text>" |
//!
//! The action commands, in the owner's chat only (spec 2026-11-09 proactive agents §7.6,
//! [`crate::actions::parse_command`]): `request @h`, `send @h|owner|group:<id> <text>`,
//! `notify <text>`, `group <name|"quoted name"> @a @b …`, `invite group:<id> @h`,
//! `call @h|owner [reason]` and `outgoing`. A malformed one answers its usage. Contacts'
//! texts are only echoed.
//!
//! The core does the rest: the answer checks, the card deadline (the backend's
//! [`Backend::answer_timeout`], "No answer; skipped"), paging long replies, skipping texts
//! older than [`MAX_TEXT_AGE`] at startup, voice calls (the owner's only), and keeping each
//! contact's question to that contact.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::actions::{self, Action, ActionRequest};
use crate::backend::{
    ActivityLabels, Answer, Backend, BackendCommand, BackendError, BackendEvent, BackendRequest,
    BackendStatus, Capability, CommandReply, ContactId, ErrorKind, PendingMessages, RequestKind,
    Responder, StartOptions, Started, SteerOutcome, TextFormat, TurnSource, unsupported,
};
use crate::config::EchoConfig;

/// The name in replies, `/status` and the tray.
pub const NAME: &str = "Echo";
/// Texts sent this long before the bridge started are not echoed.
pub const MAX_TEXT_AGE: Duration = Duration::from_secs(60);
/// The question `ask` posts.
pub const ASK_QUESTION: &str = "Red or blue?";
pub const ASK_OPTIONS: [&str; 2] = ["Red", "Blue"];
/// The question `pick` posts.
pub const PICK_QUESTION: &str = "Pick any colours.";
pub const PICK_OPTIONS: [&str; 3] = ["Red", "Blue", "Green"];
const SESSION: &str = "echo";

#[derive(Default)]
struct Inner {
    running: bool,
    busy: bool,
    /// Ends the running turn early (`interrupt`, `stop`); `true` reports it interrupted.
    cancel: Option<oneshot::Sender<bool>>,
}

/// The echo test agent.
pub struct EchoBackend {
    events: mpsc::UnboundedSender<BackendEvent>,
    answer_timeout: Duration,
    inner: Mutex<Inner>,
}

impl EchoBackend {
    /// An echo backend set up by `config`, reporting through `events`.
    pub fn new(config: EchoConfig, events: mpsc::UnboundedSender<BackendEvent>) -> Self {
        Self {
            events,
            answer_timeout: Duration::from_secs(config.answer_timeout_s),
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, event: BackendEvent) {
        let _ = self.events.send(event);
    }

    fn not_running(method: &str) -> BackendError {
        BackendError {
            method: method.to_owned(),
            code: "not_running".to_owned(),
            message: "the echo agent is not running".to_owned(),
            kind: ErrorKind::NotRunning,
        }
    }

    /// Asks `question` and waits for the answer or the turn's end; the reply to the
    /// answer, or `Err(interrupted)` when the turn was ended early.
    async fn ask(
        &self,
        question: &str,
        options: &[&str],
        multi_select: bool,
        mut cancel: oneshot::Receiver<bool>,
    ) -> Result<Option<String>, bool> {
        let (responder, answer) = Responder::new();
        self.emit(BackendEvent::Request(question_request(
            question,
            options,
            multi_select,
            responder,
        )));
        tokio::select! {
            biased;
            interrupted = &mut cancel => Err(interrupted.unwrap_or(false)),
            answer = answer => match answer {
                Ok(Answer::Question(answer)) => Ok(Some(format!("you chose: {answer}"))),
                // Declined: the core said why ("No answer; skipped", …).
                Ok(_) => Ok(None),
                // Dropped: the core closed the request itself (stopping or restarting the
                // backend, which cancels next); it ends the turn too.
                Err(_) => Err(cancel.await.unwrap_or(false)),
            },
        }
    }

    /// Asks the bridge to carry ction out and waits for its result or the turn's end;
    /// the result as one line, or Err(interrupted) when the turn was ended early.
    async fn act(
        &self,
        action: Action,
        mut cancel: oneshot::Receiver<bool>,
    ) -> Result<Option<String>, bool> {
        let (request, result) = ActionRequest::new(action);
        self.emit(BackendEvent::Action(request));
        tokio::select! {
            biased;
            interrupted = &mut cancel => Err(interrupted.unwrap_or(false)),
            result = result => Ok(Some(match result {
                Ok(result) => result.summary(),
                Err(_) => "failed: the bridge did not answer".to_owned(),
            })),
        }
    }
}

/// Echo's lines in the owner's `/help`.
pub const HELP: &str = concat!(
    "Echo's action commands (no /):\n",
    "request @handle: send a contact request\n",
    "send @handle|owner|group:<id> <text>: send a message\n",
    "notify <text>: a note in this chat\n",
    "group <name|\"quoted name\"> @a @b …: create a group of contacts\n",
    "invite group:<id> @handle: add a contact to a group\n",
    "call @handle|owner [reason]: call someone\n",
    "outgoing: the actions waiting for you"
);

/// A question with free text allowed.
fn question_request(
    question: &str,
    options: &[&str],
    multi_select: bool,
    responder: Responder,
) -> BackendRequest {
    BackendRequest {
        kind: RequestKind::Question {
            question: question.to_owned(),
            options: options.iter().map(|&option| option.to_owned()).collect(),
            multi_select,
            allow_free_text: true,
        },
        responder,
    }
}

#[async_trait::async_trait]
impl Backend for EchoBackend {
    fn name(&self) -> &str {
        NAME
    }

    fn capabilities(&self) -> BTreeSet<Capability> {
        BTreeSet::new()
    }

    fn text_format(&self) -> TextFormat {
        TextFormat::Markdown
    }

    fn answer_timeout(&self) -> Duration {
        self.answer_timeout
    }

    fn activity_labels(&self) -> ActivityLabels {
        ActivityLabels {
            working: "echoing".to_owned(),
            question: "question open".to_owned(),
        }
    }

    fn skip_texts_older_than(&self) -> Option<Duration> {
        Some(MAX_TEXT_AGE)
    }

    fn accepts_contacts(&self) -> bool {
        true
    }

    fn help(&self) -> Option<&str> {
        Some(HELP)
    }

    async fn contact_prompt(&self, contact: ContactId, text: &str) -> Result<(), BackendError> {
        let command = text.trim();
        let question = if command.eq_ignore_ascii_case("ask") {
            Some((ASK_QUESTION, &ASK_OPTIONS[..], false))
        } else if command.eq_ignore_ascii_case("pick") {
            Some((PICK_QUESTION, &PICK_OPTIONS[..], true))
        } else {
            None
        };
        let reply = match question {
            None => Some(format!("echo: {text}")),
            Some((question, options, multi_select)) => {
                let (responder, answer) = Responder::new();
                self.emit(BackendEvent::ContactRequest {
                    contact: contact.clone(),
                    request: question_request(question, options, multi_select, responder),
                });
                match answer.await {
                    Ok(Answer::Question(answer)) => Some(format!("you chose: {answer}")),
                    // Declined or closed: the core said why.
                    _ => None,
                }
            }
        };
        if let Some(text) = reply {
            self.emit(BackendEvent::ContactReply { contact, text });
        }
        Ok(())
    }

    async fn start(&self, options: StartOptions) -> Result<Started, BackendError> {
        self.lock().running = true;
        Ok(Started {
            session_id: options.session.unwrap_or_else(|| SESSION.to_owned()),
            engine_instance: None,
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            restored: None,
        })
    }

    async fn stop(&self) {
        let mut inner = self.lock();
        inner.running = false;
        if let Some(cancel) = inner.cancel.take() {
            let _ = cancel.send(false);
        }
    }

    async fn prompt(&self, text: &str) -> Result<(), BackendError> {
        let cancel = {
            let mut inner = self.lock();
            if !inner.running {
                return Err(Self::not_running("prompt"));
            }
            if inner.busy {
                return Err(BackendError {
                    method: "prompt".to_owned(),
                    code: "busy".to_owned(),
                    message: "a turn is running".to_owned(),
                    kind: ErrorKind::Busy,
                });
            }
            inner.busy = true;
            let (sender, receiver) = oneshot::channel();
            inner.cancel = Some(sender);
            receiver
        };
        self.emit(BackendEvent::TurnStarted {
            source: TurnSource::Prompt,
        });
        let command = text.trim();
        let reply = if command.eq_ignore_ascii_case("ask") {
            self.ask(ASK_QUESTION, &ASK_OPTIONS, false, cancel).await
        } else if command.eq_ignore_ascii_case("pick") {
            self.ask(PICK_QUESTION, &PICK_OPTIONS, true, cancel).await
        } else {
            match actions::parse_command(command) {
                Some(Ok(action)) => self.act(action, cancel).await,
                Some(Err(usage)) => Ok(Some(format!("usage: {usage}"))),
                None => Ok(Some(format!("echo: {text}"))),
            }
        };
        // Ends the turn under the lock, so a next prompt starts only after it.
        let mut inner = self.lock();
        match reply {
            Ok(reply) => {
                if let Some(reply) = reply {
                    self.emit(BackendEvent::TextDelta(reply));
                }
                self.emit(BackendEvent::TurnEnded { interrupted: false });
            }
            Err(true) => self.emit(BackendEvent::TurnEnded { interrupted: true }),
            // Stopped: the core ended the turn itself.
            Err(false) => {}
        }
        inner.busy = false;
        inner.cancel = None;
        Ok(())
    }

    async fn steer(&self, _text: &str) -> Result<SteerOutcome, BackendError> {
        Err(unsupported("steer"))
    }

    async fn interrupt(&self) -> Result<(), BackendError> {
        if let Some(cancel) = self.lock().cancel.take() {
            let _ = cancel.send(true);
        }
        Ok(())
    }

    async fn command(&self, _command: BackendCommand) -> Result<CommandReply, BackendError> {
        Err(unsupported("command"))
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        Ok(BackendStatus {
            model: None,
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            running_tasks: None,
        })
    }

    async fn pending_messages(
        &self,
        _after: Option<u64>,
        _engine_instance: Option<&str>,
    ) -> Result<PendingMessages, BackendError> {
        Ok(PendingMessages::default())
    }
}
