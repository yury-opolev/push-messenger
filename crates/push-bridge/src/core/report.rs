//! What the core tells the control channel: its state (published as `state_changed`
//! and into the status snapshot) and content-free events. Never message text: only
//! states, counts, names and error codes.

use push_bridge_control::{AdapterStatus, BackendInfo, BridgeState, Event, LastError};
use push_sdk::ConnectionState;

use super::Core;
use crate::backend::{BackendError, TurnSource};
use crate::cards::CardKind;
use crate::messenger::Messenger;

/// A request's kind on the wire.
fn kind_name(kind: &CardKind) -> &'static str {
    match kind {
        CardKind::Permission => "permission",
        CardKind::Question { .. } => "question",
        CardKind::Plan => "plan",
        CardKind::Bypass => "bypass",
    }
}

/// A connection state on the wire.
fn connection_name(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::Offline => "offline",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Connected => "connected",
        ConnectionState::Reconnecting { .. } => "reconnecting",
        ConnectionState::SignedOut => "signed_out",
        ConnectionState::DeviceRemoved => "device_removed",
        ConnectionState::OpenedElsewhere => "opened_elsewhere",
    }
}

impl<M: Messenger> Core<M> {
    /// The bridge's state and its detail, most important first: stopping; paused for an
    /// upgrade; the messenger connection down; the backend down; a card open (the detail
    /// is its waiting label); a turn or a slot-holding command running (no detail: tool
    /// labels can carry paths and commands); else idle.
    pub(super) fn bridge_state(&self) -> (BridgeState, Option<String>) {
        if self.stopping {
            return (BridgeState::Stopping, None);
        }
        if self.pause.is_some() {
            return (BridgeState::Paused, self.pause_detail());
        }
        if self
            .connection
            .is_some_and(|state| state != ConnectionState::Connected)
        {
            return (BridgeState::Reconnecting, None);
        }
        if self.restart_at.is_some() || self.running_since.is_none() {
            return (BridgeState::Restarting, None);
        }
        if let Some(request) = self.open.first() {
            return (BridgeState::Waiting, Some(request.waiting_label.to_owned()));
        }
        if self.turn.is_some() || self.slot_commands > 0 {
            return (BridgeState::Working, None);
        }
        (BridgeState::Idle, None)
    }

    /// Updates the status snapshot, then emits `state_changed` when the state or its
    /// detail changed.
    pub(super) fn publish(&mut self) {
        let reported = self.bridge_state();
        let changed = (self.reported.as_ref() != Some(&reported)).then(|| {
            tracing::debug!(event = "bridge_state", state = ?reported.0);
            self.reported = Some(reported.clone());
            Event::StateChanged {
                state: reported.0,
                detail: reported.1.clone(),
            }
        });
        let (state, detail) = reported;
        let folder = self.folder.display().to_string();
        let session = self.session.clone();
        let connection = self.connection.map(connection_name);
        let backend = BackendInfo {
            name: self.name.clone(),
            version: self.version.clone(),
            running: self.running_since.is_some(),
            running_tasks: self.running_tasks.len(),
        };
        let open_requests = self.open.len();
        let pause = self.pause_info();
        // A count only: the held texts themselves never leave the core.
        let held_texts = if pause.is_some() {
            self.queued.len()
        } else {
            0
        };
        let (call, call_changed) = self.report_call();
        let adapter = self.report_adapter();
        let adapter_changed = self.adapter_reports.changed(adapter.as_ref());
        self.control.update_and_emit(
            |status| {
                status.state = Some(state);
                status.detail = detail;
                status.folder = Some(folder);
                status.session = session;
                if let Some(connection) = connection {
                    status.connection = Some(connection.to_owned());
                }
                status.backend = Some(backend);
                status.open_requests = open_requests;
                status.held_texts = held_texts;
                status.pause = pause;
                status.call = call.clone();
                status.adapter = adapter;
            },
            changed,
        );
        if call_changed {
            self.control.emit(Event::CallChanged { call });
        }
        if let Some(adapter) = adapter_changed {
            self.control.emit(Event::AdapterChanged { adapter });
        }
        self.sync_call_flag();
    }

    /// Sets the call flag (if any) to [`Self::call_in_progress`].
    pub(super) fn sync_call_flag(&self) {
        let in_progress = self.call_in_progress();
        if let Some(flag) = &self.call_flag {
            flag.send_if_modified(|flag| std::mem::replace(flag, in_progress) != in_progress);
        }
    }

    /// The voice adapter with its counters, when `calls.adapter` is set.
    fn report_adapter(&mut self) -> Option<AdapterStatus> {
        let now_ms = self.clock.now_ms();
        let setup = &self.voice.as_ref()?.adapter;
        Some(self.adapter_reports.report(setup, now_ms))
    }

    pub(super) fn report_turn_started(&self, source: TurnSource) {
        let source = match source {
            TurnSource::Prompt => "prompt",
            TurnSource::Engine => "engine",
        };
        self.control.emit(Event::TurnStarted {
            source: source.to_owned(),
        });
    }

    pub(super) fn report_turn_ended(&self, interrupted: bool) {
        self.control.emit(Event::TurnEnded { interrupted });
    }

    /// A request was opened (and is in `open` now).
    pub(super) fn request_opened(&self, kind: &CardKind) {
        self.control.emit(Event::RequestOpened {
            kind: kind_name(kind).to_owned(),
            open: self.open.len(),
        });
    }

    /// A request was closed (and is no longer in `open`): "answered", "declined" or
    /// "expired".
    pub(super) fn request_closed(&self, kind: &CardKind, outcome: &str) {
        self.control.emit(Event::RequestClosed {
            kind: kind_name(kind).to_owned(),
            outcome: outcome.to_owned(),
            open: self.open.len(),
        });
    }

    /// A backend call failed: its code and method (never its message) as an `error`
    /// event and the status's last error.
    pub(super) fn report_error(&self, error: &BackendError) {
        let at = self.clock.now_ms();
        let code = error.code.clone();
        self.control.update(|status| {
            status.last_error = Some(LastError { code, at });
        });
        self.control.emit(Event::Error {
            code: error.code.clone(),
            method: Some(error.method.clone()),
        });
    }
}
