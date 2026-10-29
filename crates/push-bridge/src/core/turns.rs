//! Backend events: turns as replies, background messages and tasks, and the backend's
//! lifecycle (start, exit, restart with backoff).

use std::path::PathBuf;

use push_sdk::ClientMsgId;
use tokio::time::Instant;

use super::{Core, Turn, folder_key};
use crate::backend::{
    BackendError, BackendEvent, Capability, ErrorKind, PermissionMode, StartOptions, Started,
    TaskStatus, TurnSource,
};
use crate::backoff::RESET_AFTER;
use crate::folders;
use crate::mask::masked_line;
use crate::messenger::Messenger;
use crate::render::{ReplyUpdate, TurnRenderer, reply_bodies};

/// Characters of a background task's report shown in its reply.
const REPORT_LINE: usize = 200;

impl<M: Messenger> Core<M> {
    pub(super) async fn on_backend(&mut self, event: BackendEvent) {
        let now = Instant::now();
        match event {
            BackendEvent::TurnStarted { source } => self.begin_turn(source).await,
            BackendEvent::ThinkingDelta(delta) => self.turn().renderer.thinking(&delta),
            BackendEvent::TextDelta(delta) => self.turn().renderer.text(&delta),
            BackendEvent::ToolStarted {
                id,
                tool,
                target,
                task,
            } => self
                .turn()
                .renderer
                .tool_started(&id, &tool, &target, task.as_deref(), now),
            BackendEvent::ToolFinished { id, failed } => {
                self.turn().renderer.tool_finished(&id, failed, now);
            }
            BackendEvent::Request(request) => self.open_request(request).await,
            BackendEvent::AgentMessage {
                cursor,
                label,
                text,
            } => self.agent_message(cursor, &label, &text),
            BackendEvent::TaskStarted { background, .. } => {
                tracing::debug!(event = "task_started", background);
            }
            BackendEvent::TaskCompleted {
                description,
                status,
                background,
                report,
                ..
            } => {
                if background {
                    self.task_completed(&description, status, report.as_deref());
                }
            }
            BackendEvent::Error(message) => match &mut self.turn {
                Some(turn) => turn.renderer.note(&message),
                None => self.reply(&message),
            },
            BackendEvent::TurnEnded { interrupted } => self.end_turn(interrupted).await,
            BackendEvent::Exited { .. } => self.exited().await,
        }
    }

    /// The running turn; output outside a turn starts one.
    fn turn(&mut self) -> &mut Turn {
        self.turn.get_or_insert_with(|| Turn {
            renderer: TurnRenderer::new(Instant::now(), None),
            pages: Vec::new(),
        })
    }

    async fn begin_turn(&mut self, source: TurnSource) {
        tracing::info!(event = "turn_started", source = ?source);
        if let Some(mut turn) = self.turn.take() {
            let updates = turn.renderer.finish(false);
            self.apply(&mut turn.pages, updates);
        }
        self.turn();
        self.poll_turn();
        self.update_activity(false).await;
    }

    /// Sends what the reply has new (see [`TurnRenderer::poll`]).
    pub(super) fn poll_turn(&mut self) {
        if let Some(mut turn) = self.turn.take() {
            let updates = turn.renderer.poll(Instant::now());
            self.apply(&mut turn.pages, updates);
            self.turn = Some(turn);
        }
    }

    /// Ends the running turn's open reply before a card (see [`TurnRenderer::seal`]).
    pub(super) fn seal_turn(&mut self) {
        if let Some(mut turn) = self.turn.take() {
            let updates = turn.renderer.seal();
            self.apply(&mut turn.pages, updates);
            self.turn = Some(turn);
        }
    }

    /// Sends new pages and edits old ones.
    pub(super) fn apply(&self, pages: &mut Vec<Option<ClientMsgId>>, updates: Vec<ReplyUpdate>) {
        for update in updates {
            if pages.len() <= update.page {
                pages.resize(update.page + 1, None);
            }
            match pages[update.page] {
                Some(id) if !update.new => {
                    if let Err(error) =
                        self.messenger
                            .edit_rich(self.conversation, id, &update.body)
                    {
                        tracing::warn!(event = "edit_failed", code = error.code());
                    }
                }
                _ => pages[update.page] = self.send_rich(&update.body),
            }
        }
    }

    async fn end_turn(&mut self, interrupted: bool) {
        tracing::info!(event = "turn_ended", interrupted);
        if let Some(mut turn) = self.turn.take() {
            let updates = turn.renderer.finish(interrupted);
            self.apply(&mut turn.pages, updates);
        }
        if interrupted {
            self.close_backend_requests("Declined: the turn was stopped.");
        }
        self.update_activity(false).await;
        if std::mem::take(&mut self.new_after_turn) {
            self.new_session().await;
        }
        self.send_queued();
    }

    /// The owner's texts that waited for the turn (or a slot-holding command) to end, as
    /// one prompt.
    pub(super) fn send_queued(&mut self) {
        if self.turn.is_none() && self.slot_commands == 0 && !self.queued.is_empty() {
            let text = std::mem::take(&mut self.queued).join("\n\n");
            self.prompt(text, 0);
        }
    }

    /// A background message: its own reply titled with its label, shown once.
    fn agent_message(&self, cursor: u64, label: &str, text: &str) {
        let shown = self.state.get().agent_message_cursor;
        if shown.is_some_and(|shown| cursor <= shown) {
            return;
        }
        tracing::info!(event = "agent_message", cursor);
        for body in reply_bodies(Some(label), text) {
            self.send_rich(&body);
        }
        self.save(|state| state.agent_message_cursor = Some(cursor));
    }

    /// Background messages the bridge missed (after a reconnect or a restart against the
    /// same engine).
    pub(super) async fn fetch_pending(&mut self) {
        if !self.has(Capability::AgentMessages) || self.running_since.is_none() {
            return;
        }
        let after = self.state.get().agent_message_cursor;
        match self
            .backend
            .pending_messages(after, self.engine_instance.as_deref())
            .await
        {
            Ok(pending) => {
                tracing::info!(
                    event = "pending_messages",
                    count = pending.messages.len(),
                    lost = pending.lost
                );
                for (cursor, label, text) in &pending.messages {
                    self.agent_message(*cursor, label, text);
                }
                if pending.lost {
                    self.reply("Some background messages were lost.");
                }
            }
            Err(error) => tracing::warn!(
                event = "pending_messages_failed",
                code = error.code.as_str()
            ),
        }
    }

    /// A background task finished: a step in the running reply, else a short reply.
    fn task_completed(&mut self, description: &str, status: TaskStatus, report: Option<&str>) {
        let word = match status {
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Stopped => "stopped",
        };
        let failed = status != TaskStatus::Completed;
        match &mut self.turn {
            Some(turn) => turn
                .renderer
                .task_step(&format!("Task {description} {word}"), failed),
            None => {
                let mut text = format!("Task {description} {word}.");
                let first = report.and_then(|report| report.lines().find(|l| !l.trim().is_empty()));
                if let Some(line) = first {
                    text.push('\n');
                    text.push_str(&masked_line(line, REPORT_LINE));
                }
                self.reply(&text);
            }
        }
    }

    /// The first start: the stored folder (while still allowed) and its session.
    pub(super) async fn boot(&mut self) {
        let roots = &self.options.roots;
        let default = &self.options.default_folder;
        let allowed =
            |folder: &PathBuf| folders::resolve(&folder.to_string_lossy(), default, roots);
        let folder = self
            .state
            .get()
            .folder
            .and_then(|folder| allowed(&folder).ok())
            .or_else(|| allowed(default).ok())
            .unwrap_or_else(|| default.clone());
        self.folder = folder;
        self.start_or_retry(false).await;
    }

    /// The restart timer fired.
    pub(super) async fn restart(&mut self) {
        self.restart_at = None;
        let announce = self.announce_restart;
        self.start_or_retry(announce).await;
    }

    /// Starts the current folder's stored session; on failure, retries with backoff.
    async fn start_or_retry(&mut self, announce: bool) {
        let stored = self.stored_session(&self.folder.clone());
        let folder = self.folder.clone();
        match self
            .start_session(folder.clone(), stored, self.mode, true)
            .await
        {
            Ok(started) => {
                let was_failing = std::mem::take(&mut self.start_failing);
                self.announce_restart = false;
                self.adopt(folder, started).await;
                if announce {
                    self.reply(&self.restarted_note());
                } else if was_failing {
                    self.reply(&format!("{} started.", self.name));
                }
                self.send_queued();
            }
            Err(error) => {
                tracing::warn!(event = "backend_start_failed", code = error.code.as_str());
                if !std::mem::replace(&mut self.start_failing, true) {
                    self.reply(&format!("{error}; retrying."));
                }
                self.announce_restart = announce;
                self.schedule_restart();
            }
        }
    }

    /// "<name> restarted." and what the restart lost, by the backend's capabilities.
    fn restarted_note(&self) -> String {
        let mut lost = Vec::new();
        if self.has(Capability::AgentMessages) {
            lost.push("undelivered background messages");
        }
        if self.has(Capability::Schedules) {
            lost.push("in-memory schedules");
        }
        let mut note = format!("{} restarted.", self.name);
        if !lost.is_empty() {
            let lost = lost.join(" and ");
            let mut chars = lost.chars();
            let first = chars.next().map(|c| c.to_uppercase().to_string());
            note.push_str(&format!(
                " {}{} were lost.",
                first.unwrap_or_default(),
                chars.as_str()
            ));
        }
        note
    }

    pub(super) fn schedule_restart(&mut self) {
        let delay = self.backoff.next_delay();
        tracing::info!(
            event = "backend_restart_scheduled",
            delay_ms = delay.as_millis() as u64
        );
        self.restart_at = Some(Instant::now() + delay);
    }

    /// The backend stopped: the reply says so, its cards close, and it restarts after
    /// the backoff.
    async fn exited(&mut self) {
        tracing::warn!(event = "backend_exited");
        let note = format!("({} stopped; restarting)", self.name);
        match self.turn.take() {
            Some(mut turn) => {
                turn.renderer.note(&note);
                let updates = turn.renderer.finish(false);
                self.apply(&mut turn.pages, updates);
            }
            None => self.reply(&note),
        }
        self.close_backend_requests(&format!("Declined: {} stopped.", self.name));
        if std::mem::take(&mut self.new_after_turn) {
            // The `/new` that waited for the turn: the restart starts the new session.
            let key = folder_key(&self.folder);
            self.save(|state| {
                state.sessions.remove(&key);
            });
            self.mode = self.options.default_mode;
        }
        let now = Instant::now();
        if self
            .running_since
            .take()
            .is_some_and(|since| now.saturating_duration_since(since) >= RESET_AFTER)
        {
            self.backoff.reset();
        }
        self.announce_restart = true;
        self.schedule_restart();
        self.update_activity(false).await;
    }

    pub(super) fn stored_session(&self, folder: &std::path::Path) -> Option<String> {
        self.state.get().sessions.get(&folder_key(folder)).cloned()
    }

    /// Starts `session` in `folder`; with `fall_back`, an unknown session starts a new
    /// one (ids never move between folders).
    pub(super) async fn start_session(
        &self,
        folder: PathBuf,
        session: Option<String>,
        mode: PermissionMode,
        fall_back: bool,
    ) -> Result<Started, BackendError> {
        let options = StartOptions {
            folder,
            session,
            permission_mode: mode,
        };
        match self.backend.start(options.clone()).await {
            Err(error)
                if fall_back
                    && error.kind == ErrorKind::SessionNotFound
                    && options.session.is_some() =>
            {
                tracing::info!(event = "session_not_found");
                self.backend
                    .start(StartOptions {
                        session: None,
                        ..options
                    })
                    .await
            }
            result => result,
        }
    }

    /// Takes a started session as current and stores it; fetches missed background
    /// messages when the engine is the same as before, else starts their cursor over.
    pub(super) async fn adopt(&mut self, folder: PathBuf, started: Started) {
        tracing::info!(event = "session_started");
        let previous = self.state.get().engine_instance;
        let same_engine = started.engine_instance.is_some() && previous == started.engine_instance;
        let key = folder_key(&folder);
        let id = started.session_id.clone();
        let engine = started.engine_instance.clone();
        self.save(|state| {
            state.folder = Some(folder.clone());
            state.sessions.insert(key, id);
            if !same_engine {
                state.engine_instance = engine;
                state.agent_message_cursor = None;
            }
        });
        self.folder = folder;
        self.session = Some(started.session_id);
        self.engine_instance = started.engine_instance;
        self.version = started.version;
        self.running_since = Some(Instant::now());
        if same_engine {
            self.fetch_pending().await;
        }
    }
}
