//! The owner's commands: the core's own (`/new`, `/cd`, `/stop`, `/status`, `/help`,
//! `/mode bypass`, `/resume`) and the ones the backend runs; and the control channel's
//! actions (`restart_backend`, `reconnect`, `shutdown`, and `pause_backend` /
//! `resume_backend`, see [`super::upgrade`]).

use std::path::Path;

use push_sdk::ConnectionState;
use serde_json::json;
use tokio::sync::oneshot;

use super::{Core, Flow, Internal};
use crate::backend::{
    BackendCommand, BackendError, Capability, CommandReply, ErrorKind, PermissionMode,
};
use crate::cards;
use crate::commands::{self, Command};
use crate::control::{Action, ControlCall, Reply, error};
use crate::folders;
use crate::messenger::Messenger;

const WAIT: &str = "Wait until the turn ends.";

/// Commands that keep the agent busy without a turn: a prompt meanwhile meets a busy
/// agent.
fn holds_slot(command: &BackendCommand) -> bool {
    matches!(command, BackendCommand::Compact | BackendCommand::Fork)
}

/// The capability a command needs, when it needs one.
fn needs(command: &Command) -> Option<Capability> {
    Some(match command {
        Command::Compact => Capability::Compact,
        Command::Model(_) => Capability::Models,
        Command::Effort(_) => Capability::Effort,
        Command::Mode(_) => Capability::PermissionModes,
        Command::Goal(_) => Capability::Goal,
        Command::New | Command::Cd(_) | Command::Sessions | Command::Resume(_) => {
            Capability::Sessions
        }
        Command::Fork => Capability::Fork,
        Command::Tasks | Command::TaskStop(_) => Capability::Tasks,
        Command::Btw(_) => Capability::AskAside,
        Command::ScheduleList | Command::ScheduleAdd { .. } | Command::ScheduleRemove(_) => {
            Capability::Schedules
        }
        Command::Skills => Capability::Skills,
        _ => return None,
    })
}

/// Sends an action's result to its caller (who may have gone).
fn answer(reply: oneshot::Sender<Reply>, result: Reply) {
    let _ = reply.send(result);
}

impl<M: Messenger> Core<M> {
    /// A control-channel action. Each one answers its `reply` (now or, for a later
    /// action, once done); `shutdown` stops the run.
    pub(super) async fn on_control(&mut self, call: ControlCall) -> Flow {
        let ControlCall { action, reply } = call;
        tracing::info!(event = "control_action", action = action.name());
        match action {
            Action::RestartBackend if self.pause.is_some() => answer(
                reply,
                Err(error(
                    "paused",
                    format!("{} is paused for an upgrade: resume it instead", self.name),
                )),
            ),
            Action::RestartBackend => {
                let result = self.restart_by_owner().await;
                answer(reply, result);
            }
            Action::Reconnect => {
                let result = self.reconnect().await;
                answer(reply, result);
            }
            Action::Shutdown => {
                answer(reply, Ok(json!({"stopping": true})));
                return Flow::Stop;
            }
            Action::Pause(params) => self.pause_backend(params, reply).await,
            Action::Resume => self.resume_backend(reply).await,
            Action::Mute(muted) => {
                let result = self.mute_call(muted);
                answer(reply, result);
            }
            Action::Hangup => {
                let result = self.hang_up_call();
                answer(reply, result);
                self.update_activity(false).await;
            }
        }
        Flow::Continue
    }

    /// The owner restarts the backend: what runs is ended and declined, the backend is
    /// stopped and started again at once (no backoff); the messenger is untouched.
    async fn restart_by_owner(&mut self) -> Reply {
        tracing::info!(event = "backend_restart_by_owner");
        let note = format!("({} is restarting: the owner asked)", self.name);
        self.backend_gone(&note, &format!("Declined: {} restarted.", self.name));
        self.backend.stop().await;
        self.running_since = None;
        self.restart_at = None;
        self.announce_restart = false;
        self.publish();
        match self.start_or_retry(Some("restarted by the owner")).await {
            Ok(()) => Ok(json!({"restarted": true})),
            Err(failure) => Err(error(
                "start_failed",
                format!("{} failed ({})", failure.method, failure.code),
            )),
        }
    }

    /// Signs the messenger in again and reconnects; the backend is untouched. The next
    /// `Connected` counts as a reconnect (activity sent again, missed messages fetched).
    async fn reconnect(&mut self) -> Reply {
        tracing::info!(event = "messenger_reconnect");
        match self.messenger.reconnect().await {
            Ok(()) => {
                self.connection = Some(ConnectionState::Connecting);
                Ok(json!({"reconnected": true}))
            }
            Err(failure) => {
                tracing::warn!(event = "messenger_reconnect_failed", code = failure.code());
                Err(error(
                    failure.code(),
                    format!("signing in again failed ({})", failure.code()),
                ))
            }
        }
    }

    pub(super) async fn command(&mut self, command: Command) {
        if !self.allowed_while_paused(&command) {
            return;
        }
        if let Some(capability) = needs(&command) {
            if !self.has(capability) {
                self.reply(&format!("Not supported by {}.", self.name));
                return;
            }
        }
        let busy = self.turn.is_some();
        match command {
            Command::New if busy => {
                self.new_after_turn = true;
                self.interrupt().await;
            }
            Command::New => self.new_session().await,
            Command::Cd(_) | Command::Resume(_) | Command::Compact | Command::Fork if busy => {
                self.reply(WAIT);
            }
            Command::Cd(path) => self.change_folder(&path).await,
            Command::Resume(id) => self.resume(&id).await,
            Command::Stop if busy => self.interrupt().await,
            Command::Stop => self.reply("Nothing is running."),
            Command::Status => self.status().await,
            Command::Help => {
                let mut help = commands::help(|capability| self.has(capability));
                if let Some(more) = self.backend.help() {
                    help.push('\n');
                    help.push_str(more);
                }
                self.reply(&format!("{help}\n\n{}", commands::OWNER_NOTE));
            }
            Command::Mode(mode) if mode == "bypass" => {
                if self.mode == PermissionMode::BypassPermissions {
                    self.reply("Approvals are already off.");
                } else {
                    self.post_card(cards::bypass_card(&self.name), None).await;
                }
            }
            Command::Mode(mode) => match PermissionMode::parse(&mode) {
                Some(mode) => self.command_in_background(BackendCommand::SetPermissionMode(mode)),
                None => self.reply("Usage: /mode <default|acceptEdits|plan|bypass>"),
            },
            Command::Passthrough(text) => self.send_to_agent(text).await,
            Command::Usage(usage) => self.reply(&format!("Usage: {usage}")),
            Command::Compact => self.command_in_background(BackendCommand::Compact),
            Command::Model(None) => self.command_in_background(BackendCommand::Models),
            Command::Model(Some(model)) => {
                self.command_in_background(BackendCommand::SetModel(model));
            }
            Command::Effort(effort) => {
                self.command_in_background(BackendCommand::SetEffort(effort))
            }
            Command::Goal(goal) => {
                let goal = Some(goal).filter(|goal| !goal.trim().is_empty());
                self.command_in_background(BackendCommand::SetGoal(goal));
            }
            Command::Sessions => self.command_in_background(BackendCommand::ListSessions),
            Command::Fork => self.command_in_background(BackendCommand::Fork),
            Command::Tasks => self.command_in_background(BackendCommand::ListTasks),
            Command::TaskStop(id) => self.command_in_background(BackendCommand::StopTask(id)),
            Command::Btw(question) => {
                self.command_in_background(BackendCommand::AskAside(question));
            }
            Command::ScheduleList => self.command_in_background(BackendCommand::ScheduleList),
            Command::ScheduleAdd { when, prompt } => {
                self.command_in_background(BackendCommand::ScheduleAdd { when, prompt });
            }
            Command::ScheduleRemove(id) => {
                self.command_in_background(BackendCommand::ScheduleRemove(id));
            }
            Command::Skills => self.command_in_background(BackendCommand::Skills),
            Command::LoopList => self.loop_list(),
            Command::LoopResume(handle) => self.loop_resume(handle.as_deref()),
        }
    }

    /// Runs a backend command without waiting for it; its reply comes as
    /// [`Internal::Commanded`].
    pub(super) fn command_in_background(&mut self, command: BackendCommand) {
        if holds_slot(&command) {
            self.slot_commands += 1;
        }
        let backend = self.backend.clone();
        let internal = self.internal.clone();
        let folder = self.folder.clone();
        let session = self.session.clone();
        tokio::spawn(async move {
            let result = backend.command(command.clone()).await;
            let _ = internal.send(Internal::Commanded {
                command,
                folder,
                session,
                result,
            });
        });
    }

    /// A backend command's reply; `folder` and `session` are where it ran.
    pub(super) fn commanded(
        &mut self,
        command: BackendCommand,
        folder: &Path,
        session: Option<&str>,
        result: Result<CommandReply, BackendError>,
    ) {
        if holds_slot(&command) {
            self.slot_commands = self.slot_commands.saturating_sub(1);
        }
        let outdated = folder != self.folder || session != self.session.as_deref();
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => return self.backend_failed(&error),
        };
        match command {
            BackendCommand::SetPermissionMode(mode) if outdated => {
                tracing::info!(event = "permission_mode_outdated");
                // The change may have reached the new session's process: set the
                // current mode again.
                if mode != self.mode {
                    self.command_in_background(BackendCommand::SetPermissionMode(self.mode));
                }
                return self.reply(&format!(
                    "The change to {mode} finished after the session changed, so it was \
                     dropped; the permission mode stays {}.",
                    self.mode
                ));
            }
            BackendCommand::SetPermissionMode(mode) => {
                self.mode = mode;
                if mode == PermissionMode::BypassPermissions {
                    return self.reply("Approvals are off until /new.");
                }
            }
            BackendCommand::Fork => {
                if let Some(id) = reply.session.clone() {
                    if outdated {
                        tracing::info!(event = "fork_outdated");
                        return self.reply(&format!(
                            "The fork finished after the session changed, so it was not \
                             opened: session {id} in {}.",
                            folder.display()
                        ));
                    }
                    let key = super::folder_key(&self.folder);
                    let stored = id.clone();
                    self.save(|state| {
                        state.sessions.insert(key, stored);
                    });
                    self.session = Some(id);
                }
            }
            _ => {}
        }
        if reply.text.trim().is_empty() {
            self.reply("Done.");
        } else {
            self.reply(&reply.text);
        }
    }

    async fn interrupt(&mut self) {
        tracing::info!(event = "interrupt");
        if let Err(error) = self.backend.interrupt().await {
            self.backend_failed(&error);
        }
    }

    /// A new session in the current folder, with the default permission mode.
    pub(super) async fn new_session(&mut self) {
        let mode = self.options.default_mode;
        match self
            .start_session(self.folder.clone(), None, mode, false, false)
            .await
        {
            Ok(started) => {
                self.mode = mode;
                let id = started.session_id.clone();
                self.adopt(self.folder.clone(), started).await;
                self.close_backend_requests("Declined: a new session started.");
                self.cancel_bypass_request();
                self.reply(&format!("New session {id} in {}.", self.folder.display()));
            }
            Err(error) => self.start_failed(&error),
        }
    }

    /// `/cd`: a real folder inside the roots, with its own stored session (or a new one).
    async fn change_folder(&mut self, path: &str) {
        let folder = match folders::resolve(path, &self.folder, &self.options.roots) {
            Ok(folder) => folder,
            Err(error) => {
                let roots: Vec<String> = self
                    .options
                    .roots
                    .iter()
                    .map(|root| root.display().to_string())
                    .collect();
                return self.reply(&format!(
                    "Cannot switch to {path}: {error}. Allowed roots: {}",
                    roots.join(", ")
                ));
            }
        };
        let stored = self.stored_session(&folder);
        // Approvals turned off stay with the session they were turned off in.
        let mode = match self.mode {
            PermissionMode::BypassPermissions => self.options.default_mode,
            mode => mode,
        };
        match self
            .start_session(folder.clone(), stored.clone(), mode, true, false)
            .await
        {
            Ok(started) => {
                self.mode = mode;
                let resumed = stored.as_deref() == Some(started.session_id.as_str());
                let id = started.session_id.clone();
                self.adopt(folder, started).await;
                self.close_backend_requests("Declined: the folder changed.");
                self.cancel_bypass_request();
                let how = if resumed { "resumed" } else { "new" };
                self.reply(&format!(
                    "Folder {}; {how} session {id}.",
                    self.folder.display()
                ));
            }
            Err(error) => self.start_failed(&error),
        }
    }

    async fn resume(&mut self, id: &str) {
        match self
            .start_session(
                self.folder.clone(),
                Some(id.to_owned()),
                self.mode,
                false,
                false,
            )
            .await
        {
            Ok(started) => {
                self.adopt(self.folder.clone(), started).await;
                self.close_backend_requests("Declined: another session was resumed.");
                self.cancel_bypass_request();
                self.reply(&format!("Resumed session {id}."));
            }
            Err(error) if error.kind == ErrorKind::SessionNotFound => {
                self.reply(&format!("No session {id} in this folder."));
            }
            Err(error) => self.start_failed(&error),
        }
    }

    /// A start for a command failed: say so, and start the current session again later.
    fn start_failed(&mut self, error: &BackendError) {
        self.backend_failed(error);
        self.running_since = None;
        self.announce_restart = true;
        self.schedule_restart();
    }

    async fn status(&mut self) {
        let status = match self.backend.status().await {
            Ok(status) => status,
            Err(error) => {
                tracing::warn!(event = "status_failed", code = error.code.as_str());
                Default::default()
            }
        };
        let unknown = || "unknown".to_owned();
        let mode = match self.mode {
            PermissionMode::BypassPermissions => {
                "bypassPermissions (approvals are OFF until /new)".to_owned()
            }
            mode => mode.as_str().to_owned(),
        };
        let lines = [
            format!(
                "Session: {}",
                self.session.clone().unwrap_or_else(|| "none".to_owned())
            ),
            format!("Folder: {}", self.folder.display()),
            format!("Model: {}", status.model.unwrap_or_else(unknown)),
            format!("Permission mode: {mode}"),
            format!(
                "State: {}",
                if self.turn.is_some() { "busy" } else { "idle" }
            ),
            format!(
                "Running tasks: {}",
                status
                    .running_tasks
                    .map_or_else(unknown, |count| count.to_string())
            ),
            format!(
                "{}: {}",
                self.name,
                status
                    .version
                    .or_else(|| self.version.clone())
                    .unwrap_or_else(unknown)
            ),
        ];
        self.reply(&lines.join("\n"));
    }
}
