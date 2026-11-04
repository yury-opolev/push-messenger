//! Answering and calling policies (spec §10.4): with `calls.answer` `ask` the owner's
//! call waits for an "Answer" on a card; with `calls.notify_by_call` the bridge calls the
//! owner about a request left unanswered or urgent news (rate limited, never in the
//! do-not-disturb hours); a call that is not answered leaves the card as it was. And the
//! control channel's call actions: `mute` and `hangup`.
//!
//! Logs carry event names, reasons and codes only: never a card's text or a message.

use push_bridge_control::{CallDirection, CallInfo, CardState, NO_CALL};
use push_sdk::{CallEndReason, CallEvent, CallId, ClientMsgId, ConnectionState, OpenCard};
use serde_json::{Map, json};
use tokio::time::{Duration, Instant};

use super::voice::ActiveCall;
use super::{Core, Internal};
use crate::cards::CardKind;
use crate::control::{Reply, error};
use crate::messenger::Messenger;
use crate::render::{reply_body, rich_body, shown_line};
use crate::voice::phone::Line;
use crate::voice::policy::Verdict;
use crate::voice::speech;

/// The ask card's options: answer or decline.
const ASK_ANSWER: &str = "o1";
const ASK_DECLINE: &str = "o2";
const ASK_TITLE: &str = "Incoming call";
/// How long past the ring's end the bridge waits for the server's end of a call it asked
/// about.
const ASK_GRACE: Duration = Duration::from_secs(5);
/// The longest an ask waits, whatever the ring says.
const ASK_LONGEST: Duration = Duration::from_secs(120);
/// Characters of an urgent message's label in a note.
const NOTE_LABEL: usize = 80;

/// Why the bridge calls the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Purpose {
    /// A request has waited for the owner's answer.
    Request,
    /// An urgent background message.
    Urgent { label: String, text: String },
}

impl Purpose {
    fn name(&self) -> &'static str {
        match self {
            Purpose::Request => "request",
            Purpose::Urgent { .. } => "urgent",
        }
    }
}

/// The owner's call waiting for the owner's answer on a card.
pub(super) struct Asking {
    pub(super) call_id: CallId,
    offer_sdp: String,
    /// The card; `None` when it could not be posted.
    card: Option<ClientMsgId>,
    /// The call's events that came meanwhile.
    buffered: Vec<CallEvent>,
    deadline: Instant,
}

/// A call to the owner being placed.
pub(super) struct Dialing {
    purpose: Purpose,
    /// Hung up from the tray before the phone returned.
    cancelled: bool,
    /// Call events that came meanwhile (the call's id is not known yet).
    pub(super) buffered: Vec<CallEvent>,
}

impl<M: Messenger> Core<M> {
    /// Posts "Owner is calling — answer?" and waits for the owner's choice (or the end of
    /// the ring).
    pub(super) fn ask(&mut self, call_id: CallId, offer_sdp: String, expires_at_ms: i64) {
        let question = format!(
            "📞 Owner is calling {} in {} — answer?",
            self.name,
            self.folder_name()
        );
        let mut params = Map::new();
        params.insert("question".into(), json!(shown_line(&question, 1_000)));
        params.insert(
            "options".into(),
            json!([
                {"id": ASK_ANSWER, "label": "Answer"},
                {"id": ASK_DECLINE, "label": "Decline"},
            ]),
        );
        let body = rich_body("agent.question@1", params, &question);
        let now_ms = self.clock.now_ms();
        let ring = u64::try_from(expires_at_ms.saturating_sub(now_ms)).unwrap_or(0);
        let ring = Duration::from_millis(ring).min(ASK_LONGEST);
        let card = self.send_rich(&body);
        match card {
            Some(id) => {
                let ring_ms = i64::try_from(ring.as_millis()).unwrap_or(i64::MAX);
                self.cards.open(OpenCard {
                    conversation: self.conversation,
                    client_msg_id: id,
                    message_id: None,
                    owner: self.owner,
                    offered: vec![ASK_ANSWER.to_owned(), ASK_DECLINE.to_owned()],
                    max_values: 1,
                    deadline_ms: now_ms.saturating_add(ring_ms),
                });
            }
            None => tracing::warn!(event = "call_ask_failed"),
        }
        tracing::info!(event = "call_asking", card = card.is_some());
        self.asking = Some(Asking {
            call_id,
            offer_sdp,
            card,
            buffered: Vec::new(),
            deadline: Instant::now() + ring + ASK_GRACE,
        });
        self.sync_call_flag();
        if card.is_none() {
            self.decline_asking("📞 Declined: the card could not be shown.");
        }
    }

    /// The owner answered the ask card `card` with `values`; `false` when it is not the
    /// ask card.
    pub(super) fn ask_answered(&mut self, card: ClientMsgId, values: &[String]) -> bool {
        let Some(asking) = self.asking.take_if(|asking| asking.card == Some(card)) else {
            return false;
        };
        let answer = values.first().is_some_and(|value| value == ASK_ANSWER);
        tracing::info!(event = "call_ask_answered", answer);
        let time = self.clock.now_ms();
        self.control
            .record(|ring| ring.owner_answer(time, card, values));
        if answer {
            self.answer_call(asking.call_id, &asking.offer_sdp, asking.buffered);
        } else {
            self.decline_call(asking.call_id);
        }
        // The owner's answer shows on the card (an answered card cannot be edited).
        self.cards.close(card);
        self.peek_card(Some(card), CardState::Answered);
        true
    }

    /// A call event about the call waiting for the owner's answer: its end closes the
    /// card; the others wait for the answer.
    pub(super) async fn on_asking_event(&mut self, event: CallEvent) {
        let CallEvent::Ended { reason, .. } = event else {
            if let Some(asking) = &mut self.asking {
                asking.buffered.push(event);
            }
            return;
        };
        let Some(asking) = self.asking.take() else {
            return;
        };
        tracing::info!(event = "call_ask_ended", reason = reason.as_str());
        let text = match reason {
            CallEndReason::Missed => "📞 Missed call.",
            CallEndReason::Cancelled => "📞 The call was cancelled.",
            _ => "📞 The call ended.",
        };
        self.close_ask_card(asking.card, text, CardState::Expired);
        self.update_activity(false).await;
    }

    /// The ask's ring is over without the server saying so: the call is declined.
    pub(super) fn expire_asking(&mut self) {
        if self
            .asking
            .as_ref()
            .is_some_and(|asking| asking.deadline <= Instant::now())
        {
            tracing::info!(event = "call_ask_expired");
            self.decline_asking("📞 Missed call.");
        }
    }

    /// Declines the call waiting for the owner's answer and closes its card with `text`.
    pub(super) fn decline_asking(&mut self, text: &str) {
        if let Some(asking) = self.asking.take() {
            self.close_ask_card(asking.card, text, CardState::Declined);
            self.decline_call(asking.call_id);
        }
    }

    fn decline_call(&self, call_id: CallId) {
        let Some(voice) = &self.voice else {
            return;
        };
        let phone = voice.phone.clone();
        tokio::spawn(async move {
            if let Err(error) = phone.decline(call_id, false).await {
                tracing::debug!(event = "call_decline_failed", code = error.code());
            }
        });
    }

    /// Edits the ask card into a closed reply saying `text`.
    fn close_ask_card(&mut self, card: Option<ClientMsgId>, text: &str, state: CardState) {
        let Some(card) = card else {
            return;
        };
        self.cards.close(card);
        let body = reply_body(Some(ASK_TITLE), text, None, &[]);
        if let Err(error) = self.messenger.edit_rich(self.conversation, card, &body) {
            tracing::warn!(event = "edit_failed", code = error.code());
        }
        self.peek_card(Some(card), state);
    }

    /// Calls the owner when a request has waited `after` and the policy allows a call
    /// now; each request starts at most one call. A request whose call rings and is
    /// answered meanwhile (on screen, typed) cancels the call.
    pub(super) async fn call_owner_if_due(&mut self) {
        self.withdraw_call_if_answered();
        let Some(policy) = self.voice.as_ref().map(|voice| voice.policy.notify.clone()) else {
            return;
        };
        if policy.after.is_none() || !self.can_dial() {
            return;
        }
        let now = Instant::now();
        let due = self.open.iter().any(|request| {
            !request.notified
                && request.kind != CardKind::Bypass
                && policy.overdue(request.opened, now)
        });
        if !due || !self.dial_allowed() {
            return;
        }
        for request in &mut self.open {
            if request.kind != CardKind::Bypass && policy.overdue(request.opened, now) {
                request.notified = true;
            }
        }
        self.dial(Purpose::Request).await;
    }

    /// Calls the owner about an urgent background message when the policy wants it.
    pub(super) async fn call_owner_urgent(&mut self, label: &str, text: &str) {
        let wanted = self
            .voice
            .as_ref()
            .is_some_and(|voice| voice.policy.notify.urgent);
        if !wanted {
            return;
        }
        if !self.can_dial() || !self.dial_allowed() {
            tracing::info!(event = "urgent_call_skipped");
            return;
        }
        self.dial(Purpose::Urgent {
            label: label.to_owned(),
            text: text.to_owned(),
        })
        .await;
    }

    /// Whether the bridge could call now: voice with someone to listen and to speak (a
    /// voice adapter ready with `stt` and `tts` whose calls are advertised), connected, no
    /// call, not paused or stopping. An adapter that listens
    /// but cannot speak holds calls back (`call_held` `no_tts`, logged once per change):
    /// the owner would pick up a silent call.
    fn can_dial(&mut self) -> bool {
        let Some(voice) = &self.voice else {
            return false;
        };
        if !voice.can_listen() {
            return false;
        }
        let speaks = voice.can_speak();
        if !speaks && !self.held_no_tts {
            tracing::info!(event = "call_held", reason = "no_tts");
        }
        self.held_no_tts = !speaks;
        let advertised = voice.adapter.advertised();
        speaks
            && advertised
            && !self
                .connection
                .is_some_and(|state| state != ConnectionState::Connected)
            && !self.call_busy()
            && self.pause.is_none()
    }

    /// The rate limit and the do-not-disturb hours; logs a refusal when it changes.
    fn dial_allowed(&mut self) -> bool {
        let Some(voice) = &self.voice else {
            return false;
        };
        let verdict =
            voice
                .policy
                .notify
                .verdict(Instant::now(), self.last_dialed, voice.policy.local_now());
        if verdict != Verdict::Call && self.held_by != Some(verdict) {
            tracing::info!(event = "call_held", reason = verdict.as_str());
        }
        self.held_by = (verdict != Verdict::Call).then_some(verdict);
        verdict == Verdict::Call
    }

    /// Places a call to the owner; the phone's answer comes as [`Internal::Dialed`].
    pub(super) async fn dial(&mut self, purpose: Purpose) {
        let Some(voice) = &self.voice else {
            return;
        };
        tracing::info!(event = "call_dialing", reason = purpose.name());
        self.last_dialed = Some(Instant::now());
        let phone = voice.phone.clone();
        let owner = self.owner;
        let internal = self.internal.clone();
        self.dialing = Some(Dialing {
            purpose,
            cancelled: false,
            buffered: Vec::new(),
        });
        self.sync_call_flag();
        tokio::spawn(async move {
            let result = phone.call(owner).await;
            let _ = internal.send(Internal::Dialed { result });
        });
        self.update_activity(false).await;
    }

    /// The phone placed the call (it rings) or failed to.
    pub(super) async fn dialed(&mut self, result: Result<Line, push_sdk::SdkError>) {
        let Some(dialing) = self.dialing.take() else {
            if let Ok(line) = result {
                tokio::spawn(async move {
                    line.control.hangup().await;
                });
            }
            return;
        };
        let line = match result {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(event = "call_dial_failed", code = error.code());
                self.update_activity(false).await;
                return;
            }
        };
        let control = line.control.clone();
        let session = if dialing.cancelled || self.stopping {
            None
        } else {
            self.start_pipeline(line)
        };
        let Some(session) = session else {
            tracing::info!(event = "call_dial_cancelled");
            tokio::spawn(async move {
                control.hangup().await;
            });
            self.update_activity(false).await;
            return;
        };
        for event in &dialing.buffered {
            session.handle(event);
        }
        tracing::info!(event = "call_ringing", buffered = dialing.buffered.len());
        self.call = Some(ActiveCall::new(
            session,
            CallDirection::Outgoing,
            Some(dialing.purpose),
        ));
        self.update_activity(false).await;
    }

    /// The owner picked up the bridge's call: who calls and why, then the open card.
    pub(super) fn greet_owner_called(&self) {
        let Some(call) = &self.call else {
            return;
        };
        let hello = format!("Hi, it's {} in {}.", self.name, self.folder_name());
        match &call.purpose {
            Some(Purpose::Urgent { label, text }) => {
                call.session
                    .say(format!("{} I have something urgent.", hello));
                let label = speech::hide_secrets(label);
                call.session
                    .say(format!("{}.", label.trim_end_matches('.')));
                for sentence in speech::sentences(text) {
                    call.session.say(sentence);
                }
            }
            _ => call.session.say(hello),
        }
        match self.open.first() {
            Some(request) => {
                if call.purpose == Some(Purpose::Request) {
                    let minutes = Instant::now()
                        .saturating_duration_since(request.opened)
                        .as_secs()
                        / 60;
                    let waited = match minutes {
                        0 | 1 => "a minute".to_owned(),
                        minutes => format!("{minutes} minutes"),
                    };
                    call.session
                        .say(format!("A request has waited {waited} for your answer."));
                }
                call.session.say(request.spoken.clone());
            }
            None if call.purpose == Some(Purpose::Request) => {
                call.session
                    .say("It was answered meanwhile; nothing is waiting.");
            }
            None => {}
        }
    }

    /// The bridge's call ended before the owner picked up: what it was about stays in
    /// the chat (an open card stays open), and a note says the call was not answered.
    pub(super) fn call_unanswered(&mut self, call: ActiveCall, reason: CallEndReason) {
        if call.direction != CallDirection::Outgoing {
            return;
        }
        tracing::info!(event = "call_unanswered", reason = reason.as_str());
        if call.withdrawn {
            return;
        }
        let why = match reason {
            CallEndReason::Missed => "no answer",
            CallEndReason::Declined => "you declined",
            CallEndReason::Busy => "you were busy",
            CallEndReason::Unreachable => "you could not be reached",
            CallEndReason::Cancelled => "the call was cancelled",
            _ => "the call failed",
        };
        match call.purpose {
            Some(Purpose::Request) if !self.open.is_empty() => {
                self.reply(&format!(
                    "📞 I called about the open request ({why}). It is still open above."
                ));
            }
            Some(Purpose::Urgent { label, .. }) => {
                self.reply(&format!(
                    "📞 I called about \"{}\" ({why}).",
                    shown_line(&label, NOTE_LABEL)
                ));
            }
            _ => {}
        }
    }

    /// A call about a request that still rings is cancelled once nothing is waiting.
    fn withdraw_call_if_answered(&mut self) {
        let waiting = self
            .open
            .iter()
            .any(|request| request.kind != CardKind::Bypass);
        if waiting {
            return;
        }
        if let Some(call) = self.call.as_mut().filter(|call| {
            !call.connected && !call.withdrawn && call.purpose == Some(Purpose::Request)
        }) {
            tracing::info!(event = "call_withdrawn");
            call.withdrawn = true;
            call.session.hang_up();
        }
    }

    /// Whether a call rings, is asked about, being answered or placed, or runs, or the
    /// bridge is stopping: calls advertising waits for its end (A20) and holds still
    /// through the shutdown.
    pub(super) fn call_in_progress(&self) -> bool {
        self.call.is_some()
            || self.answering.is_some()
            || self.asking.is_some()
            || self.dialing.is_some()
            || self.stopping
    }

    /// `mute`: mutes or unmutes the agent's voice in the running call (or in the call
    /// being answered, from its start).
    pub(super) fn mute_call(&mut self, muted: bool) -> Reply {
        let Some(call) = &mut self.call else {
            if self.answering.is_some() {
                tracing::info!(event = "call_muted", muted);
                self.mute_next = muted;
                return Ok(json!({"muted": muted}));
            }
            return Err(no_call());
        };
        tracing::info!(event = "call_muted", muted);
        call.muted = muted;
        call.session.set_silenced(muted);
        Ok(json!({"muted": muted}))
    }

    /// `hangup`: hangs up the running call, cancels one being placed or ringing, declines
    /// one waiting for the owner's answer, or drops one being answered.
    pub(super) fn hang_up_call(&mut self) -> Reply {
        tracing::info!(event = "call_hangup_requested");
        if let Some(call) = &mut self.call {
            if !call.connected {
                call.withdrawn = true;
            }
            call.session.hang_up();
        } else if let Some(dialing) = &mut self.dialing {
            dialing.cancelled = true;
        } else if self.asking.is_some() {
            self.decline_asking("📞 Declined.");
        } else if self.answering.take().is_none() {
            return Err(no_call());
        }
        Ok(json!({"hungUp": true}))
    }

    /// The call for the control channel, with when its phase began, and whether it
    /// changed since the last report (then `call_changed` goes out after the snapshot).
    pub(super) fn report_call(&mut self) -> (Option<CallInfo>, bool) {
        let report = self.call_report();
        let phase = report.map(|(phase, direction, ..)| (phase, direction));
        if self.call_since.map(|(phase, _)| phase) != phase {
            let time = self.clock.now_ms();
            self.call_since = phase.map(|phase| (phase, time));
        }
        let info = report.zip(self.call_since).map(
            |((phase, direction, speaking, listening, muted), (_, since))| CallInfo {
                phase,
                direction,
                speaking,
                listening,
                muted,
                since,
            },
        );
        let changed = info != self.call_reported;
        self.call_reported = info.clone();
        (info, changed)
    }
}

fn no_call() -> push_bridge_control::ErrorBody {
    error(NO_CALL, "no call is running")
}
