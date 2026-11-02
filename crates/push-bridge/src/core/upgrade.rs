//! Upgrade mode: the owner (through the control channel) pauses the backend so its
//! program can be replaced. The backend drains and saves its state (`prepare_restart`;
//! without it, the bridge waits for the running turn), is stopped (its program file is
//! released), and starts again with its state restored once the file changed, or when
//! asked to resume. Meanwhile the owner's texts are held and sent when it is back.
//!
//! A backend that stops on its own (or fails to start) while its program is missing or
//! in use is paused the same way instead of being restarted with the backoff.

use std::path::PathBuf;
use std::time::Duration;

use push_bridge_control::{PauseInfo, PauseParams};
use serde_json::json;
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::{Core, Internal};
use crate::backend::{BackendError, ErrorKind, PermissionMode, Prepared, Restored, Started};
use crate::cards;
use crate::commands::Command;
use crate::control::{Reply, error};
use crate::exe::{self, Fingerprint, Probe};
use crate::messenger::Messenger;

/// How long the backend may drain when the caller does not say.
const DEFAULT_DRAIN: Duration = Duration::from_secs(600);
/// How often a released backend's program file is looked at.
const WATCH_EVERY: Duration = Duration::from_secs(2);
/// Characters of a pause's reason kept.
const REASON_CHARS: usize = 100;
/// Settings a restore never brings back (approvals are confirmed again).
const NOT_RESTORED: [&str; 2] = ["bypassPermissions", "permissionMode"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    /// The backend finishes its work and saves its state.
    Draining,
    /// The backend is stopped; its program file is watched.
    Released,
    /// The backend is being started again.
    Resuming,
}

/// A pause of the backend for an upgrade.
pub(super) struct Pause {
    /// Tells this pause's `prepare_restart` result from an older one's.
    id: u64,
    pub(super) phase: Phase,
    reason: Option<String>,
    since_ms: i64,
    /// The drain's end: without `prepare_restart`, the running turn is cut then.
    deadline: Instant,
    /// `Some` once `prepare_restart` succeeded.
    prepared: Option<Prepared>,
    /// `None` until `prepare_restart` answered.
    supported: Option<bool>,
    /// The work running at the release finished first.
    drained: bool,
    /// The program file at the release (`None`: unknown).
    before: Option<Fingerprint>,
    version_before: Option<String>,
    /// Approvals were off: the owner confirms them again after the restart.
    bypass_before: bool,
    /// The `pause_backend` callers, answered at the release.
    waiters: Vec<oneshot::Sender<Reply>>,
    /// The owner heard that texts are held.
    told_owner: bool,
    /// The last probe's fingerprint: the file must look the same twice in a row.
    stable: Option<Fingerprint>,
    probed: Instant,
    /// Entered after the backend stopped on its own: nothing was saved to restore.
    pub(super) killed: bool,
}

impl Pause {
    fn new(id: u64, phase: Phase, reason: Option<String>, since_ms: i64) -> Self {
        let now = Instant::now();
        Self {
            id,
            phase,
            reason,
            since_ms,
            deadline: now,
            prepared: None,
            supported: None,
            drained: true,
            before: None,
            version_before: None,
            bypass_before: false,
            waiters: Vec::new(),
            told_owner: false,
            stable: None,
            probed: now,
            killed: false,
        }
    }

    fn phase_name(&self) -> &'static str {
        match self.phase {
            Phase::Draining => "draining",
            Phase::Released if self.killed => "waiting_for_file",
            Phase::Released => "released",
            Phase::Resuming => "resuming",
        }
    }
}

/// A pause's resume whose start failed, carried to the next start that works: the
/// owner hears how the backend came back, confirms approvals that were off again, and
/// what was saved is restored.
pub(super) struct ResumeCarry {
    version_before: Option<String>,
    bypass_before: bool,
    restore: bool,
}

/// Sends an action's result to its caller (who may have gone).
fn answer(reply: oneshot::Sender<Reply>, result: Reply) {
    let _ = reply.send(result);
}

/// "1 message", "2 messages".
fn counted(count: usize, noun: &str) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {noun}{plural}")
}

/// "a", "a and b", "a, b and c".
fn joined(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// "Undelivered background messages and in-memory schedules were lost." for what a
/// restart without a restore loses; `None` when nothing.
pub(super) fn lost_sentence(lost: &[&str]) -> Option<String> {
    if lost.is_empty() {
        return None;
    }
    let lost = lost.join(" and ");
    let mut chars = lost.chars();
    let first = chars.next().map(|c| c.to_uppercase().to_string());
    Some(format!(
        "{}{} were lost.",
        first.unwrap_or_default(),
        chars.as_str()
    ))
}

/// What the owner hears when the backend is back after a pause: how it came back
/// (versions), and what it restored, or what was lost without a restore.
pub(super) fn announcement(
    name: &str,
    before: Option<&str>,
    after: Option<&str>,
    restored: Option<&Restored>,
    lost: &[&str],
) -> String {
    let head = match (before, after) {
        (Some(before), Some(after)) if before != after => {
            format!("{name} upgraded {before} → {after}")
        }
        (None, Some(after)) => format!("{name} upgraded to {after}"),
        _ => format!("{name} restarted"),
    };
    let Some(restored) = restored else {
        return match lost_sentence(lost) {
            Some(lost) => format!("{head}. {lost}"),
            None => format!("{head}."),
        };
    };
    let mut items = Vec::new();
    if restored.schedules > 0 {
        items.push(counted(restored.schedules, "schedule"));
    }
    if restored.messages > 0 {
        items.push(counted(restored.messages, "message"));
    }
    let settings: Vec<&str> = restored
        .settings
        .iter()
        .map(String::as_str)
        .filter(|setting| !NOT_RESTORED.contains(setting))
        .collect();
    if !settings.is_empty() {
        items.push(format!("the {}", settings.join("/")));
    }
    let mut text = format!("{head}: ");
    if items.is_empty() {
        text.push_str("nothing to restore");
    } else {
        text.push_str(&joined(&items));
        text.push_str(" restored");
    }
    if restored.interrupted_tasks > 0 {
        text.push_str(&format!(
            "; {} reported to {name}",
            counted(restored.interrupted_tasks, "interrupted task")
        ));
    }
    text.push('.');
    text
}

impl<M: Messenger> Core<M> {
    /// The pause on the control channel.
    pub(super) fn pause_info(&self) -> Option<PauseInfo> {
        self.pause.as_ref().map(|pause| PauseInfo {
            phase: pause.phase_name().to_owned(),
            reason: pause.reason.clone(),
            since: pause.since_ms,
        })
    }

    /// The paused state's detail.
    pub(super) fn pause_detail(&self) -> Option<String> {
        let pause = self.pause.as_ref()?;
        Some(if pause.killed {
            format!("waiting for {}'s program", self.name)
        } else {
            format!("upgrading {}", self.name)
        })
    }

    /// The activity's label while paused.
    pub(super) fn pause_label(&self) -> String {
        format!("Paused: upgrading {}", self.name)
    }

    /// While paused, a command other than `/status` and `/help` is refused (said so);
    /// `true` when `command` may run.
    pub(super) fn allowed_while_paused(&self, command: &Command) -> bool {
        if self.pause.is_none() || matches!(command, Command::Status | Command::Help) {
            return true;
        }
        self.reply(&format!(
            "{} is paused for an upgrade; try again when it is back.",
            self.name
        ));
        false
    }

    /// Holds the owner's text until the backend is back; the first one of a pause is
    /// answered.
    pub(super) fn hold(&mut self, text: String) {
        self.queued.push(text);
        let first = self
            .pause
            .as_mut()
            .is_some_and(|pause| !std::mem::replace(&mut pause.told_owner, true));
        if first {
            self.reply(&format!(
                "{} is being upgraded; your messages will be sent when it is back.",
                self.name
            ));
        }
    }

    /// `pause_backend`: starts draining (answered at the release). Already paused: a
    /// draining pause answers this caller at its release too, a released one at once.
    pub(super) async fn pause_backend(
        &mut self,
        params: PauseParams,
        reply: oneshot::Sender<Reply>,
    ) {
        if let Some(pause) = &mut self.pause {
            if pause.phase == Phase::Draining {
                pause.waiters.push(reply);
            } else {
                let phase = pause.phase_name();
                answer(reply, Ok(json!({"state": "paused", "phase": phase})));
            }
            return;
        }
        let drain = params
            .drain_timeout_ms
            .map_or(DEFAULT_DRAIN, Duration::from_millis);
        let reason = params
            .reason
            .map(|reason| reason.trim().chars().take(REASON_CHARS).collect::<String>())
            .filter(|reason| !reason.is_empty());
        tracing::info!(event = "pause_started", drain_ms = drain.as_millis() as u64);
        self.pauses += 1;
        let id = self.pauses;
        let mut pause = Pause::new(id, Phase::Draining, reason.clone(), self.clock.now_ms());
        pause.deadline = Instant::now() + drain;
        pause.waiters.push(reply);
        self.pause = Some(pause);
        self.publish();
        self.update_activity(false).await;
        if self.running_since.is_none() {
            // Nothing runs to drain.
            self.release().await;
            return;
        }
        let backend = self.backend.clone();
        let internal = self.internal.clone();
        tokio::spawn(async move {
            let result = backend.prepare_restart(drain, reason.as_deref()).await;
            let _ = internal.send(Internal::Prepared { pause: id, result });
        });
    }

    /// `prepare_restart` answered for pause `id`.
    pub(super) async fn prepared(&mut self, id: u64, result: Result<Prepared, BackendError>) {
        let Some(pause) = self.pause.as_mut().filter(|pause| pause.id == id) else {
            // A cancelled pause's: the backend saved its state, so its next start
            // restores it.
            if result.is_ok() {
                self.restore_next = true;
            }
            return;
        };
        match result {
            Ok(prepared) => {
                tracing::info!(
                    event = "backend_prepared",
                    drained = prepared.drained,
                    interrupted_tasks = prepared.interrupted_tasks
                );
                pause.supported = Some(true);
                pause.drained = prepared.drained;
                pause.prepared = Some(prepared);
                if pause.phase == Phase::Draining {
                    self.release().await;
                }
            }
            Err(failure) if failure.kind == ErrorKind::Unsupported => {
                tracing::info!(event = "prepare_unsupported");
                pause.supported = Some(false);
                self.release_when_idle().await;
            }
            Err(failure) if pause.phase == Phase::Draining => self.pause_failed(&failure).await,
            Err(failure) => tracing::warn!(
                event = "prepare_failed_after_release",
                code = failure.code.as_str()
            ),
        }
    }

    /// `prepare_restart` failed: the pause ends and the held texts are sent.
    async fn pause_failed(&mut self, failure: &BackendError) {
        tracing::warn!(
            event = "pause_failed",
            method = failure.method.as_str(),
            code = failure.code.as_str()
        );
        self.report_error(failure);
        if let Some(pause) = self.pause.take() {
            let message = format!("{} failed ({})", failure.method, failure.code);
            for waiter in pause.waiters {
                answer(waiter, Err(error("prepare_failed", message.clone())));
            }
        }
        self.reply(&format!("Pausing {} failed: {failure}", self.name));
        self.publish();
        self.update_activity(false).await;
        self.send_queued();
    }

    /// Without `prepare_restart`, a draining pause releases the backend once no turn or
    /// slot-holding command runs, or at the drain's end.
    pub(super) async fn release_when_idle(&mut self) {
        let Some(pause) = &self.pause else {
            return;
        };
        if pause.phase != Phase::Draining || pause.supported != Some(false) {
            return;
        }
        let idle = self.turn.is_none() && self.slot_commands == 0;
        if idle || Instant::now() >= pause.deadline {
            self.release().await;
        }
    }

    /// Stops the backend so its program can be replaced: what still runs is cut, its
    /// requests are declined, and the `pause_backend` callers are answered.
    pub(super) async fn release(&mut self) {
        let fingerprint = match self.backend.executable().map(|path| exe::probe(&path)) {
            Some(Probe::Usable(fingerprint)) => Some(fingerprint),
            _ => None,
        };
        let version_before = self.version.clone().or_else(|| {
            fingerprint
                .as_ref()
                .and_then(|fingerprint| fingerprint.version.clone())
        });
        let bypass_before = self.mode == PermissionMode::BypassPermissions;
        let note = format!("({} paused for an upgrade)", self.name);
        let cut = self.cut_turn(&note);
        let declined = format!("Declined: {} is being upgraded.", self.name);
        self.backend_requests_gone(&declined);
        self.backend.stop().await;
        self.running_since = None;
        self.restart_at = None;
        self.announce_restart = false;
        let Some(pause) = self.pause.as_mut() else {
            return;
        };
        pause.phase = Phase::Released;
        pause.before = fingerprint;
        pause.version_before = version_before;
        pause.bypass_before = bypass_before;
        pause.probed = Instant::now();
        if cut {
            pause.drained = false;
        }
        let interrupted = pause
            .prepared
            .as_ref()
            .map_or(0, |prepared| prepared.interrupted_tasks);
        let result = json!({
            "state": "paused",
            "prepared": pause.prepared.is_some(),
            "drained": pause.drained,
            "interruptedTasks": interrupted,
        });
        tracing::info!(
            event = "backend_released",
            prepared = pause.prepared.is_some(),
            drained = pause.drained
        );
        for waiter in pause.waiters.drain(..) {
            answer(waiter, Ok(result.clone()));
        }
        self.publish();
        self.update_activity(false).await;
    }

    /// The backend stopped while draining: it counts as released.
    pub(super) async fn exited_while_paused(&mut self) {
        let draining = self
            .pause
            .as_ref()
            .is_some_and(|pause| pause.phase == Phase::Draining);
        if draining {
            tracing::warn!(event = "backend_exited_while_draining");
            self.release().await;
        } else {
            tracing::debug!(event = "backend_exited_while_paused");
        }
    }

    /// The backend's program file is missing or in use (when it is known).
    pub(super) fn program_unusable(&self) -> bool {
        self.backend
            .executable()
            .is_some_and(|path| matches!(exe::probe(&path), Probe::Missing | Probe::Locked))
    }

    /// The backend stopped (or failed to start) and its program is missing or in use:
    /// pause and wait for the file instead of restarting with the backoff.
    pub(super) fn wait_for_program(&mut self) {
        tracing::warn!(event = "backend_program_unusable");
        self.running_since = None;
        self.restart_at = None;
        self.announce_restart = false;
        if self.pause.is_some() {
            return;
        }
        self.pauses += 1;
        let reason = format!("{}'s program is missing or in use", self.name);
        let mut pause = Pause::new(
            self.pauses,
            Phase::Released,
            Some(reason),
            self.clock.now_ms(),
        );
        pause.killed = true;
        pause.version_before = self.version.clone();
        self.pause = Some(pause);
    }

    /// Every tick: a draining pause without `prepare_restart` checks for its release;
    /// a released one looks at the program file every [`WATCH_EVERY`] and resumes once
    /// it changed (or, unknown before, is usable) and looked the same twice in a row.
    pub(super) async fn watch_pause(&mut self) {
        let phase = match &self.pause {
            Some(pause) => pause.phase,
            None => return,
        };
        match phase {
            Phase::Draining => self.release_when_idle().await,
            Phase::Released => {
                let Some(path) = self.backend.executable() else {
                    return;
                };
                let Some(pause) = self.pause.as_mut() else {
                    return;
                };
                let now = Instant::now();
                if now.saturating_duration_since(pause.probed) < WATCH_EVERY {
                    return;
                }
                pause.probed = now;
                let ready = match exe::probe(&path) {
                    Probe::Usable(fingerprint) => {
                        let changed = pause.before.as_ref() != Some(&fingerprint);
                        let stable = pause.stable.as_ref() == Some(&fingerprint);
                        pause.stable = Some(fingerprint);
                        changed && stable
                    }
                    Probe::Missing | Probe::Locked => {
                        pause.stable = None;
                        false
                    }
                };
                if ready {
                    tracing::info!(event = "program_changed");
                    self.resume_after_pause().await;
                }
            }
            Phase::Resuming => {}
        }
    }

    /// `resume_backend`: a released backend starts now when its program can be used; a
    /// draining pause is cancelled (the backend keeps running). Answered at once.
    pub(super) async fn resume_backend(&mut self, reply: oneshot::Sender<Reply>) {
        let Some(phase) = self.pause.as_ref().map(|pause| pause.phase) else {
            let message = format!("{} is not paused", self.name);
            return answer(reply, Err(error("not_paused", message)));
        };
        match phase {
            Phase::Draining => {
                answer(reply, Ok(json!({"resumed": true, "cancelled": true})));
                self.cancel_pause().await;
            }
            Phase::Resuming => answer(reply, Ok(json!({"resumed": true}))),
            Phase::Released => {
                let path = self.backend.executable();
                let usable = path
                    .as_deref()
                    .is_none_or(|path| matches!(exe::probe(path), Probe::Usable(_)));
                if !usable {
                    let file = path
                        .as_deref()
                        .and_then(|path| path.file_name())
                        .map_or_else(
                            || format!("{}'s program", self.name),
                            |file| file.to_string_lossy().into_owned(),
                        );
                    let message = format!("{file} is missing or in use");
                    return answer(reply, Err(error("exe_unavailable", message)));
                }
                answer(reply, Ok(json!({"resumed": true})));
                self.resume_after_pause().await;
            }
        }
    }

    /// Ends a draining pause without a restart: the held texts are sent.
    async fn cancel_pause(&mut self) {
        let Some(pause) = self.pause.take() else {
            return;
        };
        tracing::info!(event = "pause_cancelled");
        if pause.prepared.is_some() {
            self.restore_next = true;
        }
        for waiter in pause.waiters {
            answer(waiter, Err(error("cancelled", "the pause was cancelled")));
        }
        self.publish();
        self.update_activity(false).await;
        self.send_queued();
    }

    /// Starts the backend again after a pause, restoring what it saved, and says what
    /// came back; a failed start retries with the backoff (or waits for the program),
    /// and the next start that works restores and says so instead (see
    /// [`ResumeCarry`]).
    async fn resume_after_pause(&mut self) {
        let restore_next = self.restore_next;
        let default_mode = self.options.default_mode;
        let current_mode = self.mode;
        let carry = self.pending_resume.take();
        let Some(pause) = self.pause.as_mut() else {
            self.pending_resume = carry;
            return;
        };
        pause.phase = Phase::Resuming;
        // A pause entered after a failed resume is a killed one, yet what the earlier
        // pause saved is still there to restore.
        let saved = (pause.prepared.is_some() || restore_next) && !pause.killed;
        let restore = saved || carry.as_ref().is_some_and(|carry| carry.restore);
        let bypass_before =
            pause.bypass_before || carry.as_ref().is_some_and(|carry| carry.bypass_before);
        let mode = if bypass_before {
            default_mode
        } else {
            current_mode
        };
        let version_before = carry
            .and_then(|carry| carry.version_before)
            .or_else(|| pause.version_before.clone());
        let resume = ResumeCarry {
            version_before,
            bypass_before,
            restore,
        };
        self.publish();
        tracing::info!(event = "backend_resuming", restore);
        let folder = self.folder.clone();
        let stored = self.stored_session(&folder);
        match self
            .start_session(folder.clone(), stored, mode, true, restore)
            .await
        {
            Ok(started) => {
                self.pause = None;
                self.mode = mode;
                self.start_failing = false;
                self.back_after_pause(folder, started, resume).await;
                self.publish();
                self.update_activity(false).await;
                self.send_queued();
            }
            Err(failure) => {
                self.pause = None;
                // Approvals stay off until confirmed again, whichever start works.
                self.mode = mode;
                self.restore_next = restore;
                self.pending_resume = Some(resume);
                self.retry_start(&failure, Some("restarted"));
                self.publish();
                self.update_activity(false).await;
            }
        }
    }

    /// The backend started after a pause: takes the session and says how it came back
    /// (versions, what it restored or lost); approvals that were off are to be
    /// confirmed again.
    pub(super) async fn back_after_pause(
        &mut self,
        folder: PathBuf,
        started: Started,
        resume: ResumeCarry,
    ) {
        let before = resume.version_before;
        let file_version = match self.backend.executable().map(|path| exe::probe(&path)) {
            Some(Probe::Usable(fingerprint)) => fingerprint.version,
            _ => None,
        };
        let after = started
            .version
            .clone()
            .filter(|version| Some(version) != before.as_ref())
            .or(file_version);
        let restored = started.restored.clone();
        self.adopt(folder, started).await;
        let lost = self.lost_items();
        self.reply(&announcement(
            &self.name,
            before.as_deref(),
            after.as_deref(),
            restored.as_ref(),
            &lost,
        ));
        if resume.bypass_before {
            self.reply("Approvals were off before the upgrade; confirm to turn them off again.");
            let card = cards::bypass_card(&self.name);
            self.post_card(card, None).await;
        }
    }

    /// The bridge stops: the `pause_backend` callers still waiting hear so.
    pub(super) fn end_pause_at_close(&mut self) {
        if let Some(pause) = self.pause.take() {
            for waiter in pause.waiters {
                answer(waiter, Err(error("stopping", "the bridge stopped")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOST: [&str; 2] = ["undelivered background messages", "in-memory schedules"];

    fn restored(
        schedules: usize,
        messages: usize,
        settings: &[&str],
        interrupted: usize,
    ) -> Restored {
        Restored {
            schedules,
            messages,
            settings: settings.iter().map(|&setting| setting.to_owned()).collect(),
            interrupted_tasks: interrupted,
            steering: 0,
        }
    }

    #[test]
    fn the_head_names_both_versions_when_they_differ() {
        let restored = restored(2, 1, &["model", "goal"], 1);
        assert_eq!(
            announcement(
                "Coda",
                Some("0.1.176"),
                Some("0.1.177"),
                Some(&restored),
                &LOST
            ),
            "Coda upgraded 0.1.176 → 0.1.177: 2 schedules, 1 message and the model/goal \
             restored; 1 interrupted task reported to Coda."
        );
    }

    #[test]
    fn the_head_names_the_new_version_alone_or_says_restarted() {
        let nothing = Restored::default();
        assert_eq!(
            announcement("Coda", None, Some("0.1.177"), Some(&nothing), &LOST),
            "Coda upgraded to 0.1.177: nothing to restore."
        );
        for (before, after) in [
            (None, None),
            (Some("0.1.176"), None),
            (Some("0.1.176"), Some("0.1.176")),
        ] {
            assert_eq!(
                announcement("Coda", before, after, Some(&nothing), &LOST),
                "Coda restarted: nothing to restore.",
                "{before:?} {after:?}"
            );
        }
    }

    #[test]
    fn counts_are_singular_or_plural_and_zero_is_left_out() {
        let one = restored(1, 2, &[], 2);
        assert_eq!(
            announcement("Coda", None, None, Some(&one), &LOST),
            "Coda restarted: 1 schedule and 2 messages restored; 2 interrupted tasks \
             reported to Coda."
        );
        let messages = restored(0, 3, &[], 0);
        assert_eq!(
            announcement("Coda", None, None, Some(&messages), &LOST),
            "Coda restarted: 3 messages restored."
        );
    }

    #[test]
    fn approvals_and_the_permission_mode_are_never_announced() {
        let settings = restored(0, 0, &["bypassPermissions", "model", "permissionMode"], 0);
        assert_eq!(
            announcement("Coda", None, None, Some(&settings), &LOST),
            "Coda restarted: the model restored."
        );
        let only = restored(0, 0, &["bypassPermissions", "permissionMode"], 1);
        assert_eq!(
            announcement("Coda", None, None, Some(&only), &LOST),
            "Coda restarted: nothing to restore; 1 interrupted task reported to Coda."
        );
    }

    #[test]
    fn without_a_restore_the_losses_are_listed() {
        assert_eq!(
            announcement("Coda", Some("1.0"), Some("1.1"), None, &LOST),
            "Coda upgraded 1.0 → 1.1. Undelivered background messages and in-memory \
             schedules were lost."
        );
        assert_eq!(
            announcement("Coda", None, None, None, &LOST[1..]),
            "Coda restarted. In-memory schedules were lost."
        );
        assert_eq!(
            announcement("Coda", None, None, None, &[]),
            "Coda restarted."
        );
    }
}
