//! The owner's commands: the core's own (`/new`, `/cd`, `/stop`, `/status`, `/help`,
//! `/mode bypass`, `/resume`) and the ones the backend runs.

use std::path::Path;

use super::{Core, Internal};
use crate::backend::{
    BackendCommand, BackendError, Capability, CommandReply, ErrorKind, PermissionMode,
};
use crate::cards;
use crate::commands::{Command, HELP};
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
        Command::Sessions | Command::Resume(_) => Capability::Sessions,
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

impl<M: Messenger> Core<M> {
    pub(super) async fn command(&mut self, command: Command) {
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
            Command::Help => self.reply(HELP),
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
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => return self.backend_failed(&error),
        };
        match command {
            BackendCommand::SetPermissionMode(mode) => {
                self.mode = mode;
                if mode == PermissionMode::BypassPermissions {
                    return self.reply("Approvals are off until /new.");
                }
            }
            BackendCommand::Fork => {
                if let Some(id) = reply.session.clone() {
                    if folder != self.folder || session != self.session.as_deref() {
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
            .start_session(self.folder.clone(), None, mode, false)
            .await
        {
            Ok(started) => {
                self.mode = mode;
                let id = started.session_id.clone();
                self.adopt(self.folder.clone(), started).await;
                self.close_backend_requests("Declined: a new session started.");
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
            .start_session(folder.clone(), stored.clone(), mode, true)
            .await
        {
            Ok(started) => {
                self.mode = mode;
                let resumed = stored.as_deref() == Some(started.session_id.as_str());
                let id = started.session_id.clone();
                self.adopt(folder, started).await;
                self.close_backend_requests("Declined: the folder changed.");
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
            .start_session(self.folder.clone(), Some(id.to_owned()), self.mode, false)
            .await
        {
            Ok(started) => {
                self.adopt(self.folder.clone(), started).await;
                self.close_backend_requests("Declined: another session was resumed.");
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
