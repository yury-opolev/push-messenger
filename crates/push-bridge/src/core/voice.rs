//! Voice calls with the owner: answering them, the call's pipeline events, transcripts as
//! prompts, steers or card answers, and the agent's replies and cards spoken.
//!
//! Only the owner's calls are answered, one at a time; without anyone to listen (a voice
//! adapter ready with `stt`) every call is declined.
//! When the voice adapter goes away during a call the call stays up, chat-only, with a
//! note (A21). Logs carry event names, codes, counts and durations: never a transcript
//! or spoken text.

use std::sync::Arc;

use push_bridge_control::{CallDirection, CallPhase};
use push_sdk::{CallEvent, CallId, SdkError, UserId};
use serde_json::{Map, json};
use tokio::time::Instant;

use super::calling::Purpose;
use super::{Core, Internal, OpenRequest};
use crate::backend::Answer;
use crate::config::AnswerMode;
use crate::messenger::Messenger;
use crate::render::{reply_body, rich_body, shown, shown_line};
use crate::voice::confirm::{self, VoiceAnswer};
use crate::voice::phone::{Line, is_about};
use crate::voice::session::{EventSink, Session, SessionEvent};
use crate::voice::speech::{self, SpeechText};

/// Characters of a transcript's text.
const TRANSCRIPT: usize = 4_000;
/// Characters of a transcript's fallback.
const TRANSCRIPT_FALLBACK: usize = 2_000;
const TRANSCRIPT_PREFIX: &str = "🎤 voice: ";
/// Characters of a voice answer's label shown on its card.
const ANSWER_LABEL: usize = 200;
/// Said when the voice adapter could not recognise an utterance.
const NOT_UNDERSTOOD: &str = "Sorry, I couldn't make that out.";
/// Posted when the voice adapter went away during a call (A21).
pub(super) const ADAPTER_LOST: &str = "Voice adapter stopped; reconnecting…";
/// Posted when a voice adapter took the call again.
pub(super) const ADAPTER_BACK: &str = "Voice adapter is back.";
/// Posted when the voice adapter cannot recognise or speak during this call.
pub(super) const ADAPTER_UNAVAILABLE: &str = "Voice adapter cannot listen right now.";

/// The owner's call, answered, or the bridge's call to the owner.
pub(super) struct ActiveCall {
    pub(super) session: Session,
    /// Audio flows.
    pub(super) connected: bool,
    /// The owner speaks.
    listening: bool,
    /// The agent speaks.
    speaking: bool,
    /// The running reply's text, as spoken sentences. After a barge-in the session
    /// mutes the reply ([`Session::say_reply`]) until the core unmutes it.
    speech: SpeechText,
    /// Who called.
    pub(super) direction: CallDirection,
    /// Why the bridge called (outgoing calls).
    pub(super) purpose: Option<Purpose>,
    /// The owner muted the agent (the tray's Mute agent).
    pub(super) muted: bool,
    /// The bridge hung up a call that still rang: no note about a missed call.
    pub(super) withdrawn: bool,
}

impl ActiveCall {
    pub(super) fn new(
        session: Session,
        direction: CallDirection,
        purpose: Option<Purpose>,
    ) -> Self {
        Self {
            session,
            connected: false,
            listening: false,
            speaking: false,
            speech: SpeechText::new(),
            direction,
            purpose,
            muted: false,
            withdrawn: false,
        }
    }

    /// Speaks sentences of the agent's reply, unless a barge-in muted it.
    fn say_reply(&self, sentences: Vec<String>) {
        for sentence in sentences {
            self.session.say_reply(sentence);
        }
    }

    /// The reply is spoken again; after a barge-in, from a new sentence (an open code
    /// block or table stays silent).
    fn unmute(&mut self) {
        if self.session.unmute() {
            self.speech.interrupt();
        }
    }
}

impl<M: Messenger> Core<M> {
    /// A call event from the messenger: an incoming call is answered or declined; the
    /// other events go to the call they are about.
    pub(super) async fn on_call(&mut self, event: CallEvent) {
        if self.voice.is_none() {
            tracing::debug!(event = "call_ignored");
            return;
        }
        if let Some((call_id, buffered)) = &mut self.answering {
            if is_about(&event, *call_id) {
                buffered.push(event);
                return;
            }
        }
        if self
            .asking
            .as_ref()
            .is_some_and(|asking| is_about(&event, asking.call_id))
        {
            self.on_asking_event(event).await;
            return;
        }
        if let Some(dialing) = &mut self.dialing {
            // The call's id is not known until the phone returns: every event but a new
            // call may be about it.
            if !matches!(
                event,
                CallEvent::Incoming { .. } | CallEvent::HistoryChanged { .. }
            ) {
                dialing.buffered.push(event);
                return;
            }
        }
        if self
            .call
            .as_ref()
            .is_some_and(|call| call.session.handle(&event))
        {
            return;
        }
        match event {
            CallEvent::Incoming {
                call_id,
                caller_id,
                offer_sdp,
                expires_at_ms,
            } => self.incoming(call_id, caller_id, offer_sdp, expires_at_ms),
            _ => tracing::debug!(event = "call_event_ignored"),
        }
    }

    /// Answers the owner's call (or, with `calls.answer` `ask`, asks the owner first)
    /// unless voice is off or a call runs; declines anyone else's.
    fn incoming(&mut self, call_id: CallId, caller: UserId, offer_sdp: String, expires_at_ms: i64) {
        let Some(voice) = &self.voice else {
            return;
        };
        let refusal = if caller != self.owner {
            Some(("stranger", false))
        } else if !voice.can_listen() {
            Some(("voice_off", false))
        } else if self.call_busy() {
            Some(("busy", true))
        } else {
            None
        };
        let phone = voice.phone.clone();
        if let Some((reason, busy)) = refusal {
            tracing::info!(event = "call_refused", reason);
            tokio::spawn(async move {
                if let Err(error) = phone.decline(call_id, busy).await {
                    tracing::debug!(event = "call_decline_failed", code = error.code());
                }
            });
            return;
        }
        if voice.policy.answer == AnswerMode::Ask {
            self.ask(call_id, offer_sdp, expires_at_ms);
            return;
        }
        self.answer_call(call_id, &offer_sdp, Vec::new());
    }

    /// Whether a call runs, rings, or is being answered or placed (or the bridge stops).
    pub(super) fn call_busy(&self) -> bool {
        self.call.is_some()
            || self.answering.is_some()
            || self.asking.is_some()
            || self.dialing.is_some()
            || self.stopping
    }

    /// Answers the owner's call `call_id`; `buffered` are its events that came meanwhile.
    pub(super) fn answer_call(
        &mut self,
        call_id: CallId,
        offer_sdp: &str,
        buffered: Vec<CallEvent>,
    ) {
        let Some(voice) = &self.voice else {
            return;
        };
        tracing::info!(event = "call_answering");
        self.answering = Some((call_id, buffered));
        self.mute_next = false;
        self.sync_call_flag();
        let phone = voice.phone.clone();
        let internal = self.internal.clone();
        let offer_sdp = offer_sdp.to_owned();
        tokio::spawn(async move {
            let result = phone.answer(call_id, &offer_sdp).await;
            let _ = internal.send(Internal::Answered { call_id, result });
        });
    }

    /// The phone answered `call_id`: its pipeline starts and gets the call's events that
    /// came meanwhile.
    pub(super) async fn answered(&mut self, call_id: CallId, result: Result<Line, SdkError>) {
        let current = self
            .answering
            .as_ref()
            .is_some_and(|(answering, _)| *answering == call_id);
        if !current {
            if let Ok(line) = result {
                tracing::debug!(event = "call_answer_dropped");
                let control = line.control.clone();
                tokio::spawn(async move {
                    control.hangup().await;
                });
            }
            return;
        }
        let buffered = self
            .answering
            .take()
            .map(|(_, buffered)| buffered)
            .unwrap_or_default();
        let line = match result {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(event = "call_answer_failed", code = error.code());
                return;
            }
        };
        let Some(session) = self.start_pipeline(line) else {
            return;
        };
        for event in &buffered {
            session.handle(event);
        }
        tracing::info!(event = "call_answered", buffered = buffered.len());
        let mut call = ActiveCall::new(session, CallDirection::Incoming, None);
        if std::mem::take(&mut self.mute_next) {
            call.muted = true;
            call.session.set_silenced(true);
        }
        self.call = Some(call);
        self.update_activity(false).await;
    }

    /// Starts the pipeline of `line`, its speech in the voice adapter; its events come
    /// back as [`Internal::Voice`]. `None` (the line is dropped) without voice.
    pub(super) fn start_pipeline(&self, line: Line) -> Option<Session> {
        let voice = self.voice.as_ref()?;
        let call_id = line.call_id;
        let internal = self.internal.clone();
        let sink: EventSink = Arc::new(move |event| {
            let _ = internal.send(Internal::Voice { call_id, event });
        });
        // The owner's display name is not known to the core: the adapter gets none.
        Some(voice.start_session(line, "", sink))
    }

    /// An event of the running call's pipeline.
    pub(super) async fn on_voice(&mut self, call_id: CallId, event: SessionEvent) {
        let Some(call) = self
            .call
            .as_mut()
            .filter(|call| call.session.call_id() == call_id)
        else {
            return;
        };
        match event {
            SessionEvent::Connected => {
                call.connected = true;
                tracing::info!(event = "call_connected");
                if call.direction == CallDirection::Outgoing {
                    self.greet_owner_called();
                } else {
                    self.greet();
                }
            }
            SessionEvent::Listening(listening) => call.listening = listening,
            SessionEvent::Speaking(speaking) => call.speaking = speaking,
            // The session logs the barge-in and mutes the reply.
            SessionEvent::BargedIn => call.speech.interrupt(),
            SessionEvent::Transcript { text, started } => self.on_transcript(text, started).await,
            SessionEvent::SpeechFailed(_) => call.session.say(NOT_UNDERSTOOD),
            // The voice adapter's state during the call (A21): the session already stopped
            // or resumed the speech; the agent's replies stay in the chat meanwhile.
            SessionEvent::AdapterLost => {
                tracing::info!(event = "call_chat_only", reason = "adapter_lost");
                call.session.hush();
                self.reply(ADAPTER_LOST);
            }
            SessionEvent::AdapterBack => {
                tracing::info!(event = "call_speech_back");
                self.reply(ADAPTER_BACK);
            }
            SessionEvent::AdapterUnavailable => {
                tracing::info!(event = "call_chat_only", reason = "adapter_unavailable");
                call.session.hush();
                self.reply(ADAPTER_UNAVAILABLE);
            }
            SessionEvent::Ended(end) => {
                let call = self.call.take();
                tracing::info!(
                    event = "call_ended",
                    reason = end.reason.as_str(),
                    duration_s = end.duration_s
                );
                if let Some(call) = call.filter(|call| !call.connected) {
                    self.call_unanswered(call, end.reason);
                }
            }
        }
        self.update_activity(false).await;
    }

    /// "Connected to <name> in <folder>.", then the open card.
    fn greet(&self) {
        let Some(call) = &self.call else {
            return;
        };
        call.session.say(format!(
            "Connected to {} in {}.",
            self.name,
            self.folder_name()
        ));
        if let Some(request) = self.open.first() {
            call.session.say(request.spoken.clone());
        }
    }

    /// The last part of the current folder (all of it when it has none).
    pub(super) fn folder_name(&self) -> String {
        self.folder.file_name().map_or_else(
            || self.folder.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    }

    /// The owner said `text`, beginning at `started`: it is posted as a transcript, then
    /// answers the oldest open card, else goes to the agent (a prompt, or a steer while a
    /// turn runs; never a command). An utterance begun before the card was posted was not
    /// meant for it: it goes to the agent and the card stays open.
    async fn on_transcript(&mut self, text: String, started: Instant) {
        tracing::info!(event = "voice_transcript", chars = text.chars().count());
        self.post_transcript(&text);
        if let Some(request) = self.open.first().filter(|request| started > request.opened) {
            match confirm::match_answer(&request.kind, &text) {
                VoiceAnswer::Answer {
                    answer,
                    values,
                    label,
                } => {
                    let request = self.open.remove(0);
                    self.answer_by_voice(request, answer, &values, &label).await;
                }
                VoiceAnswer::Again(prompt) => self.say(prompt),
            }
            return;
        }
        if let Some(call) = &mut self.call {
            call.unmute();
        }
        self.send_to_agent(text).await;
    }

    /// Posts `text` as the owner's `agent.transcript@1`; in the peek it is the owner's
    /// text.
    fn post_transcript(&self, text: &str) {
        let shown_text = shown(text, TRANSCRIPT);
        if shown_text.trim().is_empty() {
            return;
        }
        let mut params = Map::new();
        params.insert("speaker".into(), json!("owner"));
        params.insert("text".into(), json!(shown_text));
        let room = TRANSCRIPT_FALLBACK - TRANSCRIPT_PREFIX.chars().count();
        let fallback = format!("{TRANSCRIPT_PREFIX}{}", shown(text, room));
        let body = rich_body("agent.transcript@1", params, &fallback);
        if let Err(error) = self.messenger.send_rich(self.conversation, &body) {
            tracing::warn!(event = "send_failed", code = error.code());
        }
        let time = self.clock.now_ms();
        self.control
            .record(|ring| ring.owner_text(time, &format!("🎤 {text}")));
    }

    /// The owner answered `request` by voice: its card becomes a closed reply saying so
    /// (the server then refuses answers to it), the request is settled, and the call
    /// hears the answer.
    async fn answer_by_voice(
        &mut self,
        request: OpenRequest,
        answer: Answer,
        values: &[String],
        label: &str,
    ) {
        tracing::info!(event = "card_answered_by_voice", values = values.len());
        let label = shown_line(label.trim_end_matches('.'), ANSWER_LABEL);
        if let Some(card) = request.card {
            let text = format!("🎤 Answered by voice: {label}.");
            let body = reply_body(Some(&request.title), &text, None, &[]);
            if let Err(error) = self.messenger.edit_rich(self.conversation, card, &body) {
                tracing::warn!(event = "edit_failed", code = error.code());
            }
            if !values.is_empty() {
                let time = self.clock.now_ms();
                self.control
                    .record(|ring| ring.owner_answer(time, card, values));
            }
        }
        let confirmation = match values {
            [value] if value == "approve" => "Approved.".to_owned(),
            [value] if value == "reject" => "Rejected.".to_owned(),
            _ => format!("Got it: {label}."),
        };
        self.settle(request, answer).await;
        if let Some(call) = &mut self.call {
            call.unmute();
            call.session.say(confirmation);
            if let Some(next) = self.open.first() {
                call.session.say(next.spoken.clone());
            }
        }
    }

    /// Says `text` in the running call.
    fn say(&self, text: &str) {
        if let Some(call) = &self.call {
            call.session.say(text);
        }
    }

    /// A turn starts: its reply is spoken from its start.
    pub(super) fn speech_turn_started(&mut self) {
        if let Some(call) = &mut self.call {
            call.speech = SpeechText::new();
            call.session.unmute();
        }
    }

    /// Speaks the sentences `delta` completes in the running reply.
    pub(super) fn speak_delta(&mut self, delta: &str) {
        if let Some(call) = self.call.as_mut().filter(|call| call.connected) {
            let sentences = call.speech.push(delta);
            call.say_reply(sentences);
        }
    }

    /// The turn ended: the rest of its reply is spoken.
    pub(super) fn speech_turn_ended(&mut self) {
        if let Some(call) = &mut self.call {
            let sentences = call.speech.finish();
            if call.connected {
                call.say_reply(sentences);
            }
        }
    }

    /// Speaks a background message: "<label>." then its text.
    pub(super) fn speak_agent_message(&self, label: &str, text: &str) {
        if let Some(call) = self.call.as_ref().filter(|call| call.connected) {
            let label = speech::hide_secrets(label);
            call.session
                .say_reply(format!("{}.", label.trim_end_matches('.')));
            call.say_reply(speech::sentences(text));
        }
    }

    /// A card opened: the reply so far is spoken, then what the card asks.
    pub(super) fn speak_card(&mut self, spoken: &str) {
        let Some(call) = self.call.as_mut().filter(|call| call.connected) else {
            return;
        };
        let sentences = call.speech.finish();
        call.say_reply(sentences);
        call.session.say(spoken);
    }

    /// The activity label during a call: "calling you" (the bridge's call rings),
    /// "speaking", "listening", "thinking" (a turn runs) or "on a call"; `None` without
    /// a call.
    pub(super) fn call_label(&self) -> Option<&'static str> {
        if self.dialing.is_some() {
            return Some("calling you");
        }
        let call = self.call.as_ref()?;
        Some(
            if !call.connected && call.direction == CallDirection::Outgoing {
                "calling you"
            } else if call.speaking {
                "speaking"
            } else if call.listening {
                "listening"
            } else if self.turn.is_some() {
                "thinking"
            } else {
                "on a call"
            },
        )
    }

    /// The call as the control channel shows it: its phase, direction and flags.
    pub(super) fn call_report(&self) -> Option<(CallPhase, CallDirection, bool, bool, bool)> {
        if self.asking.is_some() {
            return Some((
                CallPhase::Ringing,
                CallDirection::Incoming,
                false,
                false,
                false,
            ));
        }
        if self.dialing.is_some() {
            return Some((
                CallPhase::Ringing,
                CallDirection::Outgoing,
                false,
                false,
                false,
            ));
        }
        if self.answering.is_some() {
            return Some((
                CallPhase::Connecting,
                CallDirection::Incoming,
                false,
                false,
                self.mute_next,
            ));
        }
        let call = self.call.as_ref()?;
        let phase = if call.connected {
            CallPhase::InCall
        } else if call.direction == CallDirection::Outgoing {
            CallPhase::Ringing
        } else {
            CallPhase::Connecting
        };
        Some((
            phase,
            call.direction,
            call.speaking,
            call.listening,
            call.muted,
        ))
    }

    /// The bridge stops: a call being answered or placed is forgotten, and a call waiting
    /// for the owner's confirmation is declined; the running call is returned, to be shut
    /// down ([`Session::shut_down`]: the voice adapter hears of it, then the call is hung
    /// up).
    pub(super) fn end_call_at_close(&mut self) -> Option<ActiveCall> {
        self.answering = None;
        self.dialing = None;
        self.decline_asking("📞 Declined: the bridge stopped.");
        let call = self.call.take();
        if call.is_some() {
            tracing::info!(event = "call_hung_up");
        }
        call
    }
}
