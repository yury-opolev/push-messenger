//! The call router ([`AdapterEngine`], design §8, A9, A10, A21, A23): one call's speech in
//! the voice adapter.
//!
//! - When the call connects ([`SpeechEngine::connected`]), the engine takes the ready
//!   link's events ([`ReadyLink::take_events`]) and sends `call_started {call_id, peer,
//!   language}` at once, so a greeting said right after `Connected` is spoken; the
//!   `call_id` is fresh and random (A8). While the previous call's router still holds the
//!   events (it is finishing), the router waits up to [`CLAIM_WAIT`] for them before the
//!   call goes chat-only. A new link during the call (a restarted adapter) gets
//!   `call_started` again with the same `call_id` ([`SessionEvent::AdapterBack`]).
//! - The owner's audio goes to the adapter as stream 0 ([`Link::send_audio`]: its 1 s
//!   queue drops the oldest); nothing without a link (chat-only).
//! - `speech_started` → `Listening(true)` and a barge-in (the session's: the queue and the
//!   speaker are cleared, the reply muted, `BargedIn`, `Speaking(false)`) with
//!   `stop_speaking`. A final `transcript` → `Listening(false)` and
//!   `Transcript {text, started}` (`started`: its `speech_started`, else now); without
//!   `vad` the final transcript is the barge-in (A10). Partials only keep the activity
//!   "listening" (A23); a second final of an utterance is ignored; "listening" ends after
//!   [`LISTENING_TIMEOUT`] without a final.
//! - Queued sentences become `speak {speak_id: s<n>, text, interruptible}` (the bridge's
//!   own lines are not interruptible), at most [`MAX_UNFINISHED_SPEAKS`] unfinished; the
//!   rest wait in the session's queue. A sentence longer than [`MAX_TEXT_CHARS`] is split
//!   on a character boundary. Without `tts` nothing is spoken (the replies stay in the
//!   chat, as the built-in speech without TTS).
//! - Stream-1 audio belongs to the oldest unfinished speak (A9): it plays on the line's
//!   speaker in real time, at most [`TO_CALLER_BUFFER_FRAMES`] (2 s) ahead (beyond:
//!   dropped and counted). `Speaking(true)` at the first frame; `Speaking(false)` once
//!   nothing is unfinished or queued and the audio played. Audio of a speak that was
//!   stopped is dropped until its `speak_done`.
//! - `error`: `recognition_failed` → `SpeechFailed("recognition_failed")`;
//!   `synthesis_failed` → the sentence is skipped (its `speak_done` follows);
//!   `engine_unavailable` → [`SessionEvent::AdapterUnavailable`], chat-only (no audio, no
//!   speaks) until the call ends or a new adapter takes it.
//! - The link ending → [`SessionEvent::AdapterLost`] once: speech stops, chat-only.
//! - The call's end → `call_ended {call_id, reason}`, and the event receiver goes back to
//!   the supervisor ([`ReadyLink::give_back`]), which drains it again.
//!
//! Content-free (A15): logs and metrics carry ids, codes and counts, never text, audio or
//! the peer's name.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use push_sdk::CallEndReason;
use push_voice_adapter_proto::limits::{
    MAX_NAME_CHARS, MAX_TEXT_CHARS, MAX_UNFINISHED_SPEAKS, TO_CALLER_BUFFER_FRAMES,
};
use push_voice_adapter_proto::{
    AdapterMessage, AudioFrame, BridgeMessage, CallEndReason as WireEndReason, CallEnded,
    CallStarted, Capabilities, ErrorCode, ErrorMessage, Speak, SpeakDone, SpeechStarted,
    StopSpeaking, Transcript,
};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

use super::link::{Link, LinkEvent};
use super::supervisor::{ReadyLink, Supervisor};
use crate::voice::engine::{FRAME_TIME, Sentence, SpeechContext, SpeechEngine};
use crate::voice::session::SessionEvent;
use crate::voice::{FRAME, Frame};

/// "Listening" ends this long after `speech_started` without a final transcript (design
/// §5, "Slow STT").
pub const LISTENING_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a connecting call waits for the previous call's router to give the link's
/// events back.
pub const CLAIM_WAIT: Duration = Duration::from_secs(2);
/// How far ahead stream-1 audio is queued on the line's speaker (A12).
const TO_CALLER_BUFFER: Duration =
    Duration::from_millis(FRAME_TIME.as_millis() as u64 * TO_CALLER_BUFFER_FRAMES as u64);
/// Utterances remembered (their `speech_started`, their final).
const REMEMBERED_UTTERANCES: usize = 64;

/// What a call needs to run its speech in the voice adapter.
#[derive(Clone, Debug)]
pub struct AdapterSetup {
    pub supervisor: Supervisor,
    /// `calls.language`, sent in each `call_started`.
    pub language: String,
    /// The router's counters, over every call.
    pub metrics: Arc<AdapterMetrics>,
}

impl AdapterSetup {
    pub fn new(supervisor: Supervisor, language: impl Into<String>) -> Self {
        Self {
            supervisor,
            language: language.into(),
            metrics: Arc::default(),
        }
    }

    /// Whether a ready adapter can listen (`stt`): calls can be taken.
    pub fn can_listen(&self) -> bool {
        self.supervisor
            .state()
            .borrow()
            .ready()
            .is_some_and(|info| info.capabilities.stt)
    }
}

/// Last, average and maximum of a latency.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Latency {
    pub count: u64,
    pub last: Duration,
    pub total: Duration,
    pub max: Duration,
}

impl Latency {
    pub fn record(&mut self, latency: Duration) {
        self.count += 1;
        self.last = latency;
        self.total += latency;
        self.max = self.max.max(latency);
    }

    pub fn average(&self) -> Option<Duration> {
        u32::try_from(self.count)
            .ok()
            .filter(|count| *count > 0)
            .map(|count| self.total / count)
    }

    fn add(&mut self, other: &Latency) {
        if other.count == 0 {
            return;
        }
        self.count += other.count;
        self.last = other.last;
        self.total += other.total;
        self.max = self.max.max(other.max);
    }

    fn last_ms(&self) -> Option<u64> {
        (self.count > 0).then_some(self.last.as_millis() as u64)
    }

    fn max_ms(&self) -> Option<u64> {
        (self.count > 0).then_some(self.max.as_millis() as u64)
    }
}

/// The router's counters: of one call, or summed over calls ([`AdapterMetrics`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Metrics {
    /// Calls the adapter took (`call_started` sent).
    pub calls: u64,
    /// The owner's frames given to the link (stream 0).
    pub frames_to_adapter: u64,
    /// Stream-1 frames played on the line.
    pub frames_from_adapter: u64,
    /// Stream-1 frames dropped: of a stopped speak, beyond the 2 s buffer, without a speak.
    pub frames_dropped: u64,
    /// `speak`s sent.
    pub speaks: u64,
    /// A final transcript → the first `speak` after it.
    pub final_to_speak: Latency,
    /// A `speak` → its first stream-1 frame.
    pub speak_to_audio: Latency,
}

impl Metrics {
    fn add(&mut self, call: &Metrics) {
        self.calls += call.calls;
        self.frames_to_adapter += call.frames_to_adapter;
        self.frames_from_adapter += call.frames_from_adapter;
        self.frames_dropped += call.frames_dropped;
        self.speaks += call.speaks;
        self.final_to_speak.add(&call.final_to_speak);
        self.speak_to_audio.add(&call.speak_to_audio);
    }
}

/// The router's counters over every call, shared with `status` and the shutdown log.
#[derive(Debug, Default)]
pub struct AdapterMetrics(Mutex<Metrics>);

impl AdapterMetrics {
    pub fn snapshot(&self) -> Metrics {
        *lock(&self.0)
    }

    fn record_call(&self, call: &Metrics) {
        lock(&self.0).add(call);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where the call is, for the router's task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Ringing,
    Connected,
    Ended(WireEndReason),
}

/// A `speak` the adapter has not finished.
struct Unfinished {
    speak_id: String,
    generation: u64,
    sent: Instant,
    /// Its first stream-1 frame arrived.
    sounded: bool,
    /// `stop_speaking` covered it: its audio is dropped.
    stopped: bool,
}

/// What the engine's handle and the router's task share.
#[derive(Default)]
struct State {
    /// The call took a link or found none ([`Inner::start_call`], [`Router::lose`]); until
    /// then sentences wait (the greeting comes as the call connects, maybe while the
    /// router waits for the previous call to give the link's events back).
    settled: bool,
    /// The link `call_started` went to; `None` before, and while chat-only after a loss.
    link: Option<Link>,
    capabilities: Capabilities,
    /// `engine_unavailable`: no audio, no speaks.
    unavailable: bool,
    /// Oldest first.
    unfinished: VecDeque<Unfinished>,
    next_speak: u64,
    /// The latest final transcript, until a `speak` follows.
    last_final: Option<Instant>,
    call: Metrics,
}

impl State {
    fn speaks(&self) -> bool {
        self.link.is_some() && self.capabilities.tts && !self.unavailable
    }

    /// Whether sentences may be queued: speaking, or not settled yet.
    fn may_speak(&self) -> bool {
        !self.settled || self.speaks()
    }
}

/// A link the call took and its events.
type Claimed = (ReadyLink, mpsc::UnboundedReceiver<LinkEvent>);

struct Inner {
    ctx: Arc<SpeechContext>,
    call_id: String,
    peer: String,
    language: String,
    metrics: Arc<AdapterMetrics>,
    state: Mutex<State>,
    phase: watch::Sender<Phase>,
    /// The link [`SpeechEngine::connected`] took, for the router's task to adopt.
    pending: Mutex<Option<Claimed>>,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn send(&self, link: &Link, message: BridgeMessage) {
        if let Err(error) = link.send(message) {
            tracing::warn!(event = "adapter_message_invalid", %error);
        }
    }

    /// Takes `ready`'s events if nobody holds them and its link lives: `call_started`.
    fn claim_now(&self, ready: &ReadyLink) -> Option<Claimed> {
        let events = ready.take_events()?;
        if ready.link().end().is_some() {
            ready.give_back(events);
            return None;
        }
        self.start_call(ready);
        Some((ready.clone(), events))
    }

    /// Sends `call_started` on `ready`'s link: the call speaks there from now on.
    fn start_call(&self, ready: &ReadyLink) {
        let link = ready.link().clone();
        let capabilities = link.info().capabilities;
        self.send(
            &link,
            BridgeMessage::CallStarted(CallStarted {
                call_id: self.call_id.clone(),
                peer: self.peer.clone(),
                language: self.language.clone(),
            }),
        );
        {
            let mut state = self.state();
            state.settled = true;
            state.link = Some(link);
            state.capabilities = capabilities;
            state.unavailable = false;
            state.unfinished.clear();
            state.call.calls = 1;
        }
        tracing::info!(
            event = "adapter_call_started",
            call_id = %self.call_id,
            link = ready.id()
        );
        self.ctx.wake.notify_one();
    }

    /// Sends `stop_speaking` for every unfinished speak not stopped yet.
    fn stop_unfinished(&self, state: &mut State) {
        if state.unfinished.iter().all(|speak| speak.stopped) {
            return;
        }
        for speak in &mut state.unfinished {
            speak.stopped = true;
        }
        if let Some(link) = &state.link {
            self.send(
                link,
                BridgeMessage::StopSpeaking(StopSpeaking {
                    call_id: self.call_id.clone(),
                    speak_id: None,
                }),
            );
        }
    }
}

/// Speech in the voice adapter for one call (see the module's documentation).
pub struct AdapterEngine {
    inner: Arc<Inner>,
    current: watch::Receiver<Option<ReadyLink>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl AdapterEngine {
    /// Starts the router's task on the current runtime; it takes the adapter once the
    /// call connects.
    pub(crate) fn start(ctx: Arc<SpeechContext>, setup: &AdapterSetup, peer: &str) -> Self {
        let inner = Arc::new(Inner {
            ctx,
            call_id: new_call_id(),
            peer: display_name(peer),
            language: setup.language.clone(),
            metrics: setup.metrics.clone(),
            state: Mutex::default(),
            phase: watch::channel(Phase::Ringing).0,
            pending: Mutex::default(),
        });
        let current = setup.supervisor.current();
        let task = tokio::spawn(route(
            inner.clone(),
            inner.phase.subscribe(),
            current.clone(),
        ));
        Self {
            inner,
            current,
            task: Mutex::new(Some(task)),
        }
    }

    fn end_with(&self, reason: WireEndReason) {
        self.inner.phase.send_if_modified(|phase| {
            if matches!(phase, Phase::Ended(_)) {
                return false;
            }
            *phase = Phase::Ended(reason);
            true
        });
    }
}

#[async_trait::async_trait]
impl SpeechEngine for AdapterEngine {
    fn hear(&self, frame: &Frame) {
        let mut state = self.inner.state();
        if state.unavailable {
            return;
        }
        if let Some(link) = &state.link {
            link.send_audio(frame);
            state.call.frames_to_adapter += 1;
        }
    }

    fn speaks(&self) -> bool {
        self.inner.state().may_speak()
    }

    /// Takes the ready link at once (`call_started`), unless the previous call's router
    /// still holds its events: then the router's task waits for them.
    fn connected(&self) {
        let ready = self.current.borrow().clone();
        // Under the phase's lock: the router's task adopts what was taken once it sees
        // the phase change (even straight to an end).
        self.inner.phase.send_if_modified(|phase| {
            if *phase != Phase::Ringing {
                return false;
            }
            if let Some(claimed) = ready.as_ref().and_then(|ready| self.inner.claim_now(ready)) {
                *lock(&self.inner.pending) = Some(claimed);
            }
            *phase = Phase::Connected;
            true
        });
    }

    fn stop(&self) {
        let mut state = self.inner.state();
        self.inner.stop_unfinished(&mut state);
        drop(state);
        self.inner.ctx.wake.notify_one();
    }

    async fn end(&self, reason: CallEndReason) {
        self.end_with(wire_reason(reason));
        let task = lock(&self.task).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    async fn shut_down(&self) {
        self.end_with(WireEndReason::Shutdown);
        let task = lock(&self.task).take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for AdapterEngine {
    /// The session was dropped (it hangs up): the call ended.
    fn drop(&mut self) {
        self.end_with(WireEndReason::Completed);
    }
}

/// How the call ended, for the adapter (never `other`, which is never sent): a call
/// that was up ends `completed` or `failed`; the bridge stopping is
/// [`SpeechEngine::shut_down`]'s `shutdown`.
fn wire_reason(reason: CallEndReason) -> WireEndReason {
    match reason {
        CallEndReason::Completed => WireEndReason::Completed,
        _ => WireEndReason::Failed,
    }
}

/// A fresh random call id: 16 hex digits, unrelated to the server's call id (A8).
fn new_call_id() -> String {
    let mut id = uuid::Uuid::new_v4().simple().to_string();
    id.truncate(16);
    id
}

/// The peer's display name as the protocol takes it: no control characters, at most
/// [`MAX_NAME_CHARS`] characters.
fn display_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_NAME_CHARS)
        .collect()
}

/// `text` cut after at most [`MAX_TEXT_CHARS`] characters: the head and the rest, if any.
fn split_text(mut text: String) -> (String, Option<String>) {
    match text.char_indices().nth(MAX_TEXT_CHARS) {
        Some((at, _)) => {
            let rest = text.split_off(at);
            (text, Some(rest))
        }
        None => (text, None),
    }
}

/// The router's task: waits for the call to connect, routes until it ends, then ends the
/// adapter's call.
async fn route(
    inner: Arc<Inner>,
    mut phase: watch::Receiver<Phase>,
    mut current: watch::Receiver<Option<ReadyLink>>,
) {
    let mut router = Router {
        inner,
        ready: None,
        events: None,
        lost: false,
        listening_until: None,
        started: HashMap::new(),
        finals: VecDeque::new(),
    };
    loop {
        let now = *phase.borrow_and_update();
        match now {
            Phase::Ringing => {}
            Phase::Connected => break,
            Phase::Ended(reason) => {
                // Connected and ended at once: the adapter's call is ended too.
                if router.adopt() {
                    router.finish(reason);
                }
                return;
            }
        }
        if phase.changed().await.is_err() {
            return;
        }
    }
    if !router.adopt() {
        let ready = current.borrow_and_update().clone();
        match ready {
            Some(ready) => {
                let events = tokio::select! {
                    events = ready.wait_for_events(CLAIM_WAIT) => events,
                    reason = ended(&mut phase) => return router.finish(reason),
                };
                match events {
                    Some(events) => router.attach_with(ready, events),
                    None => {
                        tracing::warn!(event = "adapter_events_taken", link = ready.id());
                        router.lose();
                    }
                }
            }
            None => router.lose(),
        }
    }
    let mut supervised = true;
    let reason = loop {
        let playing = router.pump();
        let deadline = match (playing, router.listening_until) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        tokio::select! {
            changed = phase.changed() => {
                let now = *phase.borrow();
                match (changed, now) {
                    (Err(_), _) => break WireEndReason::Completed,
                    (Ok(()), Phase::Ended(reason)) => break reason,
                    _ => {}
                }
            }
            changed = current.changed(), if supervised => match changed {
                Ok(()) => {
                    let ready = current.borrow_and_update().clone();
                    router.on_ready(ready);
                }
                Err(_) => supervised = false,
            },
            event = next_event(&mut router.events) => router.on_event(event).await,
            () = router.inner.ctx.wake.notified() => {}
            () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                router.on_timer();
            }
        }
    };
    router.finish(reason);
}

/// How the call ended, once it has.
async fn ended(phase: &mut watch::Receiver<Phase>) -> WireEndReason {
    let Ok(now) = phase
        .wait_for(|phase| matches!(phase, Phase::Ended(_)))
        .await
    else {
        return WireEndReason::Completed;
    };
    match *now {
        Phase::Ended(reason) => reason,
        _ => WireEndReason::Completed,
    }
}

/// The next link event; `None` once the channel closed; pending without a link.
async fn next_event(events: &mut Option<mpsc::UnboundedReceiver<LinkEvent>>) -> Option<LinkEvent> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

/// The router task's own state.
struct Router {
    inner: Arc<Inner>,
    /// The link the call runs on, and its events.
    ready: Option<ReadyLink>,
    events: Option<mpsc::UnboundedReceiver<LinkEvent>>,
    /// [`SessionEvent::AdapterLost`] was reported, no link since.
    lost: bool,
    /// "Listening" since a `speech_started` (or a partial), until then.
    listening_until: Option<Instant>,
    /// When each utterance's `speech_started` arrived.
    started: HashMap<String, Instant>,
    /// Utterances whose final arrived, oldest first.
    finals: VecDeque<String>,
}

impl Router {
    fn emit(&self, event: SessionEvent) {
        self.inner.ctx.emit(event);
    }

    /// Adopts the link [`SpeechEngine::connected`] took; whether there was one.
    fn adopt(&mut self) -> bool {
        let Some((ready, events)) = lock(&self.inner.pending).take() else {
            return false;
        };
        self.ready = Some(ready);
        self.events = Some(events);
        true
    }

    /// Takes a new `ready` for the call: `call_started` (again, after a loss).
    fn attach(&mut self, ready: ReadyLink) {
        let Some(events) = ready.take_events() else {
            tracing::warn!(event = "adapter_events_taken", link = ready.id());
            return self.lose();
        };
        self.attach_with(ready, events);
    }

    /// Takes `ready`, whose `events` the router holds, for the call.
    fn attach_with(&mut self, ready: ReadyLink, events: mpsc::UnboundedReceiver<LinkEvent>) {
        if ready.link().end().is_some() {
            ready.give_back(events);
            return self.lose();
        }
        self.inner.start_call(&ready);
        self.ready = Some(ready);
        self.events = Some(events);
        if std::mem::take(&mut self.lost) {
            self.emit(SessionEvent::AdapterBack);
        }
    }

    /// The link went away (or there was none): chat-only until a new one.
    fn lose(&mut self) {
        self.ready = None;
        self.events = None;
        {
            let mut state = self.inner.state();
            state.settled = true;
            state.link = None;
            state.unfinished.clear();
        }
        self.inner.ctx.stop_speaking(false);
        self.stop_listening();
        self.started.clear();
        if !std::mem::replace(&mut self.lost, true) {
            tracing::info!(event = "adapter_lost", call_id = %self.inner.call_id);
            self.emit(SessionEvent::AdapterLost);
        }
    }

    fn stop_listening(&mut self) {
        if self.listening_until.take().is_some() {
            self.emit(SessionEvent::Listening(false));
        }
    }

    /// The supervisor published a link (or none).
    fn on_ready(&mut self, ready: Option<ReadyLink>) {
        let current = self.ready.as_ref().map(ReadyLink::id);
        match ready {
            Some(ready) if Some(ready.id()) != current => {
                if self.ready.is_some() {
                    self.lose();
                }
                self.attach(ready);
            }
            Some(_) => {}
            None => {
                let ended = self
                    .ready
                    .as_ref()
                    .is_some_and(|ready| ready.link().end().is_some());
                if ended {
                    self.lose();
                }
            }
        }
    }

    async fn on_event(&mut self, event: Option<LinkEvent>) {
        match event {
            Some(LinkEvent::Message(message)) => self.on_message(message),
            Some(LinkEvent::Audio(frame)) => self.on_audio(frame).await,
            // A closed channel without `Closed`: the supervisor drained it.
            Some(LinkEvent::Closed(_)) | None => self.lose(),
        }
    }

    fn ours(&self, call_id: &str) -> bool {
        call_id == self.inner.call_id
    }

    fn on_message(&mut self, message: AdapterMessage) {
        match message {
            AdapterMessage::SpeechStarted(SpeechStarted {
                call_id,
                utterance_id,
            }) => {
                if !self.ours(&call_id) {
                    return;
                }
                if self.started.len() >= REMEMBERED_UTTERANCES {
                    self.started.clear();
                }
                self.started.insert(utterance_id, Instant::now());
                self.barge_in();
                self.emit(SessionEvent::Listening(true));
                self.listening_until = Some(Instant::now() + LISTENING_TIMEOUT);
            }
            AdapterMessage::Transcript(transcript) => self.on_transcript(transcript),
            AdapterMessage::SpeakDone(SpeakDone {
                call_id, speak_id, ..
            }) => {
                if !self.ours(&call_id) {
                    return;
                }
                let mut state = self.inner.state();
                if let Some(at) = state
                    .unfinished
                    .iter()
                    .position(|speak| speak.speak_id == speak_id)
                {
                    state.unfinished.remove(at);
                }
            }
            AdapterMessage::Error(error) => self.on_error(error),
            AdapterMessage::Hello(_) | AdapterMessage::Ping(_) | AdapterMessage::Pong(_) => {}
        }
    }

    fn on_transcript(&mut self, transcript: Transcript) {
        if !self.ours(&transcript.call_id) {
            return;
        }
        if !transcript.is_final {
            if self.listening_until.is_none() {
                self.emit(SessionEvent::Listening(true));
                self.listening_until = Some(Instant::now() + LISTENING_TIMEOUT);
            }
            return;
        }
        if self.finals.contains(&transcript.utterance_id) {
            return;
        }
        if self.finals.len() >= REMEMBERED_UTTERANCES {
            self.finals.pop_front();
        }
        let now = Instant::now();
        let started = self.started.remove(&transcript.utterance_id).unwrap_or(now);
        self.finals.push_back(transcript.utterance_id);
        if !self.inner.state().capabilities.vad {
            self.barge_in();
        }
        self.listening_until = None;
        self.emit(SessionEvent::Listening(false));
        let text = transcript.text.trim();
        if text.is_empty() {
            return;
        }
        self.inner.state().last_final = Some(now);
        self.emit(SessionEvent::Transcript {
            text: text.to_owned(),
            started,
        });
    }

    fn on_error(&mut self, error: ErrorMessage) {
        if error
            .call_id
            .as_deref()
            .is_some_and(|call_id| !self.ours(call_id))
        {
            return;
        }
        let code = error.code.as_str();
        match error.code {
            ErrorCode::RecognitionFailed => {
                if let Some(utterance_id) = &error.utterance_id {
                    self.started.remove(utterance_id);
                }
                self.stop_listening();
                tracing::info!(event = "adapter_recognition_failed", call_id = %self.inner.call_id);
                self.emit(SessionEvent::SpeechFailed(code.to_owned()));
            }
            ErrorCode::SynthesisFailed => {
                tracing::info!(
                    event = "adapter_synthesis_failed",
                    call_id = %self.inner.call_id,
                    speak_id = error.speak_id.as_deref().unwrap_or("")
                );
            }
            ErrorCode::EngineUnavailable => {
                {
                    let mut state = self.inner.state();
                    if state.unavailable || state.link.is_none() {
                        return;
                    }
                    self.inner.stop_unfinished(&mut state);
                    state.unavailable = true;
                }
                self.inner.ctx.stop_speaking(false);
                self.stop_listening();
                tracing::info!(event = "adapter_unavailable", call_id = %self.inner.call_id);
                self.emit(SessionEvent::AdapterUnavailable);
            }
            _ => tracing::info!(event = "adapter_error", code),
        }
    }

    /// The owner speaks: cut the agent's speech (the session's barge-in) and the
    /// adapter's.
    fn barge_in(&mut self) {
        if self.inner.ctx.stop_speaking(true) {
            tracing::info!(event = "voice_barge_in");
        }
        let mut state = self.inner.state();
        self.inner.stop_unfinished(&mut state);
    }

    /// A stream-1 frame: the oldest unfinished speak's (A9).
    async fn on_audio(&mut self, frame: AudioFrame) {
        let ctx = &self.inner.ctx;
        let generation = {
            let mut state = self.inner.state();
            let generation = ctx.generation();
            let playable = state
                .unfinished
                .front()
                .is_some_and(|speak| !speak.stopped && speak.generation == generation);
            if !playable {
                state.call.frames_dropped += 1;
                return;
            }
            let now = Instant::now();
            let mut latency = None;
            if let Some(speak) = state.unfinished.front_mut() {
                if !std::mem::replace(&mut speak.sounded, true) {
                    latency = Some(now.saturating_duration_since(speak.sent));
                }
            }
            if let Some(latency) = latency {
                state.call.speak_to_audio.record(latency);
            }
            generation
        };
        let Ok(samples) = <[i16; FRAME]>::try_from(frame.samples.as_slice()) else {
            self.inner.state().call.frames_dropped += 1;
            return;
        };
        {
            let mut speech = ctx.speech();
            let full = speech
                .playing_until
                .is_some_and(|until| until >= Instant::now() + TO_CALLER_BUFFER);
            if ctx.ended() || generation != ctx.generation() || full {
                drop(speech);
                self.inner.state().call.frames_dropped += 1;
                return;
            }
            if !std::mem::replace(&mut speech.speaking, true) {
                ctx.emit(SessionEvent::Speaking(true));
            }
        }
        if !ctx.speaker.send(samples).await {
            return;
        }
        {
            let mut speech = ctx.speech();
            if generation != ctx.generation() {
                drop(speech);
                // A barge-in or a hush overtook the frame while the speaker took it (see
                // the built-in engine): clear again.
                ctx.speaker.clear();
                self.inner.state().call.frames_dropped += 1;
                return;
            }
            let now = Instant::now();
            let from = speech.playing_until.map_or(now, |until| until.max(now));
            speech.playing_until = Some(from + FRAME_TIME);
        }
        self.inner.state().call.frames_from_adapter += 1;
    }

    fn on_timer(&mut self) {
        if self
            .listening_until
            .is_some_and(|until| until <= Instant::now())
        {
            tracing::info!(event = "adapter_listening_timeout", call_id = %self.inner.call_id);
            self.stop_listening();
        }
    }

    /// Sends queued sentences while fewer than [`MAX_UNFINISHED_SPEAKS`] are unfinished
    /// and reports `Speaking(false)` once everything played; when the playing audio ends
    /// (to wait for).
    fn pump(&mut self) -> Option<Instant> {
        let inner = &self.inner;
        let ctx = &inner.ctx;
        let mut state = inner.state();
        let generation = ctx.generation();
        let speaks = state.speaks();
        let mut speech = ctx.speech();
        if !speaks {
            speech.queue.clear();
        }
        while state.unfinished.len() < MAX_UNFINISHED_SPEAKS {
            let Some(Sentence {
                generation: queued,
                text,
                own,
            }) = speech.queue.pop_front()
            else {
                break;
            };
            if queued != generation || text.trim().is_empty() {
                continue;
            }
            let (text, rest) = split_text(text);
            if let Some(rest) = rest {
                speech.queue.push_front(Sentence {
                    generation,
                    text: rest,
                    own,
                });
            }
            let Some(link) = state.link.clone() else {
                break;
            };
            state.next_speak += 1;
            let speak_id = format!("s{}", state.next_speak);
            inner.send(
                &link,
                BridgeMessage::Speak(Speak {
                    call_id: inner.call_id.clone(),
                    speak_id: speak_id.clone(),
                    text,
                    interruptible: !own,
                    voice: None,
                }),
            );
            let now = Instant::now();
            if let Some(final_at) = state.last_final.take() {
                state
                    .call
                    .final_to_speak
                    .record(now.saturating_duration_since(final_at));
            }
            state.call.speaks += 1;
            state.unfinished.push_back(Unfinished {
                speak_id,
                generation,
                sent: now,
                sounded: false,
                stopped: false,
            });
        }
        let busy = state
            .unfinished
            .iter()
            .any(|speak| !speak.stopped && speak.generation == generation);
        speech.busy = busy.then_some(generation);
        if !speech.speaking || busy || !speech.queue.is_empty() || ctx.ended() {
            return None;
        }
        match speech.playing_until {
            Some(until) if until > Instant::now() => Some(until),
            _ => {
                speech.speaking = false;
                speech.playing_until = None;
                ctx.emit(SessionEvent::Speaking(false));
                None
            }
        }
    }

    /// The call ended: `call_ended`, the events back to the supervisor, the metrics.
    fn finish(mut self, reason: WireEndReason) {
        let call = {
            let mut state = self.inner.state();
            state.unfinished.clear();
            if let Some(link) = state.link.take() {
                self.inner.send(
                    &link,
                    BridgeMessage::CallEnded(CallEnded {
                        call_id: self.inner.call_id.clone(),
                        reason,
                    }),
                );
            }
            state.call
        };
        if let (Some(ready), Some(events)) = (self.ready.take(), self.events.take()) {
            ready.give_back(events);
        }
        self.inner.metrics.record_call(&call);
        tracing::info!(
            event = "adapter_metrics",
            call_id = %self.inner.call_id,
            frames_to_adapter = call.frames_to_adapter,
            frames_from_adapter = call.frames_from_adapter,
            frames_dropped = call.frames_dropped,
            speaks = call.speaks,
            final_to_speak_last_ms = call.final_to_speak.last_ms(),
            final_to_speak_max_ms = call.final_to_speak.max_ms(),
            speak_to_audio_last_ms = call.speak_to_audio.last_ms(),
            speak_to_audio_max_ms = call.speak_to_audio.max_ms(),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicU64, Ordering};

    use push_sdk::CallId;
    use push_voice_adapter_proto::{
        AdapterHello, AsyncFrameReader, AsyncFrameWriter, BridgeHello, Decoded, Frame as WireFrame,
        Pong, Stream, decode, encode,
    };
    use push_voice_adapter_testkit::tones::{self, SPEAK_HZ};
    use push_voice_adapter_testkit::{Event, Log, Script, Word};
    use tokio::time::{sleep, timeout};

    use super::*;
    use crate::voice::adapter::supervisor::testing::{Plan, ScriptedStarter};
    use crate::voice::adapter::supervisor::{Started, Starter};
    use crate::voice::phone::{FakeCall, FakePhone, LineEnded, Phone, Speaker};
    use crate::voice::session::{EventSink, Session};

    const WORD_HZ: f64 = 1000.0;
    const SAMPLES_PER_FRAME: usize = FRAME;

    fn script() -> Script {
        Script::with_words(vec![Word::new(WORD_HZ, "approve")])
    }

    /// The owner's word: a tone between silences.
    fn word() -> Vec<i16> {
        let mut audio = tones::silence(5);
        audio.extend(tones::tone(WORD_HZ, 20));
        audio.extend(tones::silence(25));
        audio
    }

    /// Records when frames reach the line's speaker.
    struct Recorder {
        inner: Arc<dyn Speaker>,
        sent: Mutex<Vec<Instant>>,
    }

    #[async_trait::async_trait]
    impl Speaker for Recorder {
        async fn send(&self, frame: Frame) -> bool {
            lock(&self.sent).push(Instant::now());
            self.inner.send(frame).await
        }

        fn clear(&self) {
            self.inner.clear();
        }
    }

    struct Call {
        session: Session,
        owner: FakeCall,
        events: mpsc::UnboundedReceiver<SessionEvent>,
        supervisor: Supervisor,
        setup: AdapterSetup,
        recorder: Arc<Recorder>,
    }

    async fn start_call(starter: Arc<dyn Starter>) -> Call {
        open_call(ready_supervisor(starter).await).await
    }

    /// A supervisor of `starter`, once its adapter is ready.
    async fn ready_supervisor(starter: Arc<dyn Starter>) -> Supervisor {
        let supervisor = Supervisor::with_starter(starter, BridgeHello::v1("1.2.3", "en-US"));
        let mut current = supervisor.current();
        timeout(Duration::from_secs(10), current.wait_for(Option::is_some))
            .await
            .expect("the adapter gets ready")
            .unwrap();
        supervisor
    }

    /// A call on `supervisor`'s adapter, once connected.
    async fn open_call(supervisor: Supervisor) -> Call {
        let setup = AdapterSetup::new(supervisor.clone(), "en-US");
        let phone = FakePhone::new();
        let mut line = phone.answer(CallId::new(), "offer").await.unwrap();
        let owner = phone.wait_answered().await;
        let recorder = Arc::new(Recorder {
            inner: line.speaker.clone(),
            sent: Mutex::default(),
        });
        line.speaker = recorder.clone();
        let (sender, events) = mpsc::unbounded_channel();
        let sink: EventSink = Arc::new(move |event| {
            let _ = sender.send(event);
        });
        let session = Session::with_adapter(line, &setup, "Owner\nName", sink);
        let mut call = Call {
            session,
            owner,
            events,
            supervisor,
            setup,
            recorder,
        };
        call.until(&SessionEvent::Connected).await;
        call
    }

    /// A call on the testkit's adapter playing `scripts` in turn.
    async fn scripted_call(scripts: Vec<Script>) -> (Call, Arc<ScriptedStarter>, Log) {
        let starter = ScriptedStarter::new(scripts.into_iter().map(Plan::Adapter));
        let call = start_call(starter.clone()).await;
        let log = starter.logs()[0].clone();
        wait_log(&log, |event| matches!(event, Event::CallStarted { .. })).await;
        (call, starter, log)
    }

    async fn wait_log(log: &Log, matches: impl Fn(&Event) -> bool) -> Instant {
        timeout(Duration::from_secs(30), log.wait_for(matches))
            .await
            .expect("the adapter's event")
            .at
    }

    impl Call {
        /// The events up to and including `wanted`; panics after 60 s.
        async fn until(&mut self, wanted: &SessionEvent) -> Vec<SessionEvent> {
            let mut seen = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                match tokio::time::timeout_at(deadline, self.events.recv()).await {
                    Ok(Some(event)) => {
                        let found = same(&event, wanted);
                        seen.push(event);
                        if found {
                            return seen;
                        }
                    }
                    _ => panic!("no {wanted:?} in {seen:?}"),
                }
            }
        }

        /// The events that arrive within `time`.
        async fn during(&mut self, time: Duration) -> Vec<SessionEvent> {
            let mut seen = Vec::new();
            let deadline = Instant::now() + time;
            while let Ok(Some(event)) = tokio::time::timeout_at(deadline, self.events.recv()).await
            {
                seen.push(event);
            }
            seen
        }
    }

    fn transcript(text: &str) -> SessionEvent {
        SessionEvent::Transcript {
            text: text.into(),
            started: Instant::now(),
        }
    }

    fn same(event: &SessionEvent, wanted: &SessionEvent) -> bool {
        match (event, wanted) {
            (
                SessionEvent::Transcript { text, .. },
                SessionEvent::Transcript { text: wanted, .. },
            ) => text == wanted,
            _ => event == wanted,
        }
    }

    fn call_id(log: &Log) -> String {
        log.events()
            .into_iter()
            .find_map(|observed| match observed.event {
                Event::CallStarted { call_id } => Some(call_id),
                _ => None,
            })
            .expect("call_started")
    }

    fn speaks(log: &Log) -> Vec<(String, bool, usize)> {
        log.events()
            .into_iter()
            .filter_map(|observed| match observed.event {
                Event::Speak {
                    speak_id,
                    interruptible,
                    chars,
                    ..
                } => Some((speak_id, interruptible, chars)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn the_owners_word_becomes_a_transcript_at_once() {
        let (mut call, _starter, log) = scripted_call(vec![script()]).await;
        let id = call_id(&log);
        assert_eq!(id.len(), 16);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
        let owner = call.owner.clone();
        let speaking = tokio::spawn(async move { owner.say_paced(&word()).await });
        let seen = call.until(&transcript("approve")).await;
        let arrived = Instant::now();
        speaking.await.unwrap();
        assert_eq!(
            seen[..2],
            [
                SessionEvent::Listening(true),
                SessionEvent::Listening(false)
            ],
            "{seen:?}"
        );
        let SessionEvent::Transcript { started, .. } = seen[2] else {
            panic!("{seen:?}");
        };
        let speech_started = wait_log(&log, |e| matches!(e, Event::SpeechStartedSent { .. })).await;
        let final_sent = wait_log(&log, |e| {
            matches!(e, Event::TranscriptSent { is_final: true, .. })
        })
        .await;
        assert!(started >= speech_started, "the onset is speech_started's");
        assert!(started - speech_started < Duration::from_millis(20));
        assert!(arrived - final_sent < Duration::from_millis(100));
        assert!(log.count(|e| matches!(e, Event::Audio { .. })) >= 40);
        assert!(!call.session.speaking());
    }

    #[tokio::test(start_paused = true)]
    async fn a_sentence_is_spoken_with_the_adapters_voice_without_gaps() {
        let (mut call, _starter, log) = scripted_call(vec![script()]).await;
        call.session.say_reply("one two three four five");
        call.session.say_reply("six seven eight");
        assert!(call.session.speaking());
        assert_eq!(
            call.until(&SessionEvent::Speaking(true)).await,
            vec![SessionEvent::Speaking(true)]
        );
        assert_eq!(
            call.until(&SessionEvent::Speaking(false)).await,
            vec![SessionEvent::Speaking(false)],
            "one Speaking(true) … Speaking(false) for both"
        );
        assert!(!call.session.speaking());
        // 23 + 15 characters at 40 ms each.
        let frames = 46 + 30;
        let heard = call.owner.heard();
        assert_eq!(heard.len(), frames * SAMPLES_PER_FRAME);
        assert!(tones::amplitude(&heard, SPEAK_HZ) > 4000.0);
        assert_eq!(
            speaks(&log),
            vec![("s1".into(), true, 23), ("s2".into(), true, 15)]
        );
        let sent = lock(&call.recorder.sent).clone();
        assert_eq!(sent.len(), frames);
        let gap = sent.windows(2).map(|w| w[1] - w[0]).max().unwrap();
        assert!(gap <= Duration::from_millis(40), "a gap of {gap:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_bridges_own_lines_are_not_interruptible() {
        let (mut call, _starter, log) = scripted_call(vec![script()]).await;
        call.session.say("Hi.");
        call.until(&SessionEvent::Speaking(false)).await;
        assert_eq!(speaks(&log), vec![("s1".into(), false, 3)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_barge_in_stops_the_adapter_and_silences_the_call_at_once() {
        let (mut call, _starter, log) = scripted_call(vec![script()]).await;
        call.session.say_reply("a".repeat(200));
        call.session.say_reply("b".repeat(200));
        call.until(&SessionEvent::Speaking(true)).await;
        sleep(Duration::from_millis(300)).await;
        let owner = call.owner.clone();
        let speaking = tokio::spawn(async move { owner.say_paced(&word()).await });
        let seen = call.until(&SessionEvent::BargedIn).await;
        let barged_in = Instant::now();
        assert!(!seen.contains(&SessionEvent::Speaking(false)), "{seen:?}");
        assert_eq!(
            call.until(&SessionEvent::Listening(true)).await,
            vec![SessionEvent::Speaking(false), SessionEvent::Listening(true)]
        );
        let speech_started = wait_log(&log, |e| matches!(e, Event::SpeechStartedSent { .. })).await;
        let stopped = wait_log(&log, |e| matches!(e, Event::StopSpeaking { .. })).await;
        assert!(barged_in - speech_started <= Duration::from_millis(40));
        assert!(stopped - speech_started <= Duration::from_millis(40));
        assert!(call.owner.clears() >= 1);
        let heard = call.owner.heard().len();
        assert!(heard < 30 * SAMPLES_PER_FRAME, "heard {heard} samples");
        call.until(&transcript("approve")).await;
        speaking.await.unwrap();
        assert_eq!(
            call.owner.heard().len(),
            heard,
            "nothing after the barge-in"
        );
        assert!(!call.session.speaking());
        // The reply is muted until the core unmutes it; the bridge's own lines are spoken.
        call.session.say_reply("muted");
        assert!(call.during(Duration::from_millis(300)).await.is_empty());
        call.session.say("one two");
        call.until(&SessionEvent::Speaking(true)).await;
        call.until(&SessionEvent::Speaking(false)).await;
        let after = call.owner.heard_since_clear();
        assert_eq!(after.len(), 14 * SAMPLES_PER_FRAME);
    }

    #[tokio::test(start_paused = true)]
    async fn twenty_sentences_keep_at_most_sixteen_unfinished() {
        let (mut call, _starter, log) = scripted_call(vec![script()]).await;
        for _ in 0..20 {
            call.session.say_reply("one two");
        }
        let done = |e: &Event| {
            matches!(
                e,
                Event::SpeakDoneSent {
                    interrupted: false,
                    ..
                }
            )
        };
        timeout(Duration::from_secs(60), log.wait_count(20, done))
            .await
            .expect("every sentence spoken");
        let (mut unfinished, mut most) = (0usize, 0usize);
        for observed in log.events() {
            match observed.event {
                Event::Speak { .. } => unfinished += 1,
                Event::SpeakDoneSent { .. } => unfinished -= 1,
                _ => {}
            }
            most = most.max(unfinished);
        }
        assert_eq!(most, MAX_UNFINISHED_SPEAKS);
        assert_eq!(speaks(&log).len(), 20);
        let seen = call.during(Duration::from_secs(1)).await;
        assert_eq!(
            seen,
            vec![SessionEvent::Speaking(true), SessionEvent::Speaking(false)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_adapter_leaves_the_call_chat_only_until_a_new_one_takes_it() {
        let (mut call, starter, log) = scripted_call(vec![script(), script()]).await;
        let id = call_id(&log);
        call.session.say_reply("a".repeat(200));
        call.until(&SessionEvent::Speaking(true)).await;
        sleep(Duration::from_millis(200)).await;
        starter.crash();
        assert_eq!(
            call.until(&SessionEvent::AdapterLost).await,
            vec![SessionEvent::Speaking(false), SessionEvent::AdapterLost]
        );
        let heard = call.owner.heard().len();
        assert!(!call.session.speaking());
        call.session.say_reply("into the chat");
        assert!(!call.session.speaking());
        call.owner.say(&tones::tone(WORD_HZ, 10)).await;

        assert_eq!(
            call.until(&SessionEvent::AdapterBack).await,
            vec![SessionEvent::AdapterBack]
        );
        assert_eq!(call.owner.heard().len(), heard, "silence while lost");
        let second = starter.logs()[1].clone();
        wait_log(&second, |e| matches!(e, Event::CallStarted { .. })).await;
        assert_eq!(call_id(&second), id, "the same call_id again");
        assert!(
            !second
                .events()
                .iter()
                .any(|observed| matches!(observed.event, Event::Speak { .. })),
            "nothing from before the loss is spoken"
        );
        call.session.say("back");
        call.until(&SessionEvent::Speaking(true)).await;
        call.until(&SessionEvent::Speaking(false)).await;
        call.owner.say_paced(&word()).await;
        call.until(&transcript("approve")).await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_end_of_the_call_ends_the_adapters_and_gives_the_events_back() {
        let (mut call, _starter, log) = scripted_call(vec![script()]).await;
        call.session.say_reply("a".repeat(100));
        call.until(&SessionEvent::Speaking(true)).await;
        call.owner.end(CallEndReason::Completed);
        let seen = call
            .until(&SessionEvent::Ended(LineEnded {
                reason: CallEndReason::Completed,
                duration_s: Some(0),
            }))
            .await;
        assert_eq!(seen[0], SessionEvent::Speaking(false), "{seen:?}");
        let ended = log
            .wait_for(|e| matches!(e, Event::CallEnded { .. }))
            .await
            .event;
        assert_eq!(
            ended,
            Event::CallEnded {
                call_id: call_id(&log),
                reason: "completed".into()
            }
        );
        let ready = call.supervisor.current().borrow().clone().unwrap();
        assert!(ready.take_events().is_some(), "given back");
        let metrics = call.setup.metrics.snapshot();
        assert_eq!((metrics.calls, metrics.speaks), (1, 1));
        assert!(metrics.frames_from_adapter > 0);
        assert_eq!(metrics.speak_to_audio.count, 1);
    }

    /// Where the first event `matches` is in the adapter's log.
    fn position(log: &Log, matches: impl Fn(&Event) -> bool) -> Option<usize> {
        log.events()
            .iter()
            .position(|observed| matches(&observed.event))
    }

    fn is_speak(event: &Event) -> bool {
        matches!(event, Event::Speak { .. })
    }

    fn is_call_started(event: &Event) -> bool {
        matches!(event, Event::CallStarted { .. })
    }

    #[tokio::test(start_paused = true)]
    async fn a_greeting_said_as_the_call_connects_is_spoken() {
        let starter = ScriptedStarter::new([Plan::Adapter(script())]);
        let mut call = start_call(starter.clone()).await;
        // At `Connected`, without waiting for the adapter's `call_started`.
        call.session.say("Hello.");
        let log = starter.logs()[0].clone();
        wait_log(&log, is_speak).await;
        assert!(position(&log, is_call_started) < position(&log, is_speak));
        assert_eq!(speaks(&log), vec![("s1".into(), false, 6)]);
        call.until(&SessionEvent::Speaking(true)).await;
        let seen = call.until(&SessionEvent::Speaking(false)).await;
        assert!(!seen.contains(&SessionEvent::AdapterLost), "{seen:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_bridge_stop_ends_the_adapters_call_as_a_shutdown() {
        let (call, starter, log) = scripted_call(vec![script()]).await;
        let Call {
            session,
            supervisor,
            ..
        } = call;
        session.shut_down().await;
        timeout(Duration::from_secs(10), supervisor.shutdown())
            .await
            .expect("the supervisor stops");
        let ended: Vec<Event> = log
            .events()
            .into_iter()
            .filter_map(|observed| match observed.event {
                event @ Event::CallEnded { .. } => Some(event),
                _ => None,
            })
            .collect();
        assert_eq!(
            ended,
            vec![Event::CallEnded {
                call_id: call_id(&log),
                reason: "shutdown".into()
            }]
        );
        for task in starter.take_serving() {
            let outcome = timeout(Duration::from_secs(10), task).await.unwrap();
            assert_eq!(
                outcome.unwrap().unwrap(),
                push_voice_adapter_testkit::Outcome::Closed
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_waits_for_the_previous_call_to_give_the_adapter_back() {
        let starter = ScriptedStarter::new([Plan::Adapter(script())]);
        let supervisor = ready_supervisor(starter.clone()).await;
        // The previous call's router, still finishing, holds the events.
        let ready = supervisor.current().borrow().clone().unwrap();
        let events = ready.take_events().unwrap();
        tokio::spawn(async move {
            sleep(Duration::from_millis(300)).await;
            ready.give_back(events);
        });
        let mut call = open_call(supervisor).await;
        call.session.say("Hello.");
        let log = starter.logs()[0].clone();
        wait_log(&log, is_speak).await;
        let seen = call.until(&SessionEvent::Speaking(false)).await;
        assert!(!seen.contains(&SessionEvent::AdapterLost), "{seen:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn back_to_back_calls_both_get_the_adapter() {
        let (call, _starter, log) = scripted_call(vec![script()]).await;
        let first = call_id(&log);
        let Call {
            session,
            supervisor,
            ..
        } = call;
        drop(session);
        let mut call = open_call(supervisor).await;
        call.session.say("Hello.");
        timeout(Duration::from_secs(10), log.wait_count(2, is_call_started))
            .await
            .expect("the second call_started");
        wait_log(&log, is_speak).await;
        let seen = call.until(&SessionEvent::Speaking(false)).await;
        assert!(!seen.contains(&SessionEvent::AdapterLost), "{seen:?}");
        let ended = position(&log, |e| {
            *e == Event::CallEnded {
                call_id: first.clone(),
                reason: "completed".into(),
            }
        })
        .expect("the first call ended");
        let second = log
            .events()
            .iter()
            .enumerate()
            .filter(|(_, observed)| is_call_started(&observed.event))
            .map(|(at, _)| at)
            .nth(1)
            .unwrap();
        assert!(ended < second);
        assert!(
            !log.events()
                .iter()
                .any(|observed| matches!(&observed.event, Event::ErrorSent { .. }))
        );
    }

    #[test]
    fn every_end_reason_is_one_the_protocol_sends() {
        use CallEndReason::*;
        for reason in [
            Completed,
            Missed,
            Declined,
            Cancelled,
            Busy,
            Failed,
            Unreachable,
        ] {
            assert_ne!(wire_reason(reason), WireEndReason::Other, "{reason:?}");
        }
        assert_eq!(wire_reason(Completed), WireEndReason::Completed);
        assert_eq!(wire_reason(Failed), WireEndReason::Failed);
    }

    /// A hand-driven adapter: the test sends its messages and reads the bridge's.
    struct Manual {
        received: mpsc::UnboundedReceiver<BridgeMessage>,
        send: mpsc::UnboundedSender<WireFrame>,
        audio: Arc<AtomicU64>,
    }

    struct ManualStarter {
        capabilities: Capabilities,
        adapters: mpsc::UnboundedSender<Manual>,
    }

    #[async_trait::async_trait]
    impl Starter for ManualStarter {
        async fn start(&self) -> io::Result<Started> {
            let (bridge_reader, adapter_writer) = tokio::io::duplex(1 << 20);
            let (adapter_reader, bridge_writer) = tokio::io::duplex(1 << 20);
            let (to_test, received) = mpsc::unbounded_channel();
            let (send, mut from_test) = mpsc::unbounded_channel::<WireFrame>();
            let audio = Arc::new(AtomicU64::new(0));
            let counted = audio.clone();
            let capabilities = self.capabilities;
            tokio::spawn(async move {
                let mut reader = AsyncFrameReader::new(adapter_reader);
                let mut writer = AsyncFrameWriter::new(adapter_writer);
                loop {
                    let Ok(Some(frame)) = reader.read_frame().await else {
                        return;
                    };
                    if let WireFrame::Control(json) = frame {
                        if let Ok(Decoded::Known(BridgeMessage::Hello(_))) = decode(&json) {
                            break;
                        }
                    }
                }
                let hello = AdapterHello::new("manual", "1", capabilities);
                if writer
                    .write_message(&AdapterMessage::Hello(hello))
                    .await
                    .is_err()
                {
                    return;
                }
                loop {
                    let written = tokio::select! {
                        frame = reader.read_frame() => match frame {
                            Ok(Some(WireFrame::Control(json))) => match decode::<BridgeMessage>(&json) {
                                Ok(Decoded::Known(BridgeMessage::Ping(ping))) => {
                                    writer.write_message(&AdapterMessage::Pong(Pong { id: ping.id })).await
                                }
                                Ok(Decoded::Known(message)) => {
                                    let _ = to_test.send(message);
                                    Ok(())
                                }
                                _ => Ok(()),
                            },
                            Ok(Some(WireFrame::Audio(_))) => {
                                counted.fetch_add(1, Ordering::Relaxed);
                                Ok(())
                            }
                            _ => return,
                        },
                        frame = from_test.recv() => match frame {
                            Some(frame) => writer.write_frame(&frame).await,
                            None => return,
                        },
                    };
                    if written.is_err() {
                        return;
                    }
                }
            });
            let _ = self.adapters.send(Manual {
                received,
                send,
                audio,
            });
            Ok(Started {
                reader: Box::new(bridge_reader),
                writer: Box::new(bridge_writer),
                child: None,
            })
        }
    }

    impl Manual {
        /// The bridge's next message but pings.
        async fn next(&mut self) -> BridgeMessage {
            timeout(Duration::from_secs(10), self.received.recv())
                .await
                .expect("a message from the bridge")
                .expect("the bridge is connected")
        }

        /// Nothing from the bridge for a second.
        async fn nothing(&mut self) {
            if let Ok(Some(message)) = timeout(Duration::from_secs(1), self.received.recv()).await {
                panic!("unexpected {message:?}");
            }
        }

        fn say(&self, message: AdapterMessage) {
            self.send.send(encode(&message).unwrap()).unwrap();
        }

        /// A stream-1 frame of the voice.
        fn sound(&self, seq: u32) {
            self.send
                .send(WireFrame::Audio(AudioFrame {
                    stream: Stream::ToCaller,
                    seq,
                    timestamp_ms: seq * 20,
                    samples: tones::tone_frame(SPEAK_HZ, 6000.0, seq as usize).to_vec(),
                }))
                .unwrap();
        }

        fn speak_done(&self, call_id: &str, speak_id: &str, interrupted: bool) {
            self.say(AdapterMessage::SpeakDone(SpeakDone {
                call_id: call_id.into(),
                speak_id: speak_id.into(),
                interrupted,
            }));
        }

        fn speech_started(&self, call_id: &str, utterance_id: &str) {
            self.say(AdapterMessage::SpeechStarted(SpeechStarted {
                call_id: call_id.into(),
                utterance_id: utterance_id.into(),
            }));
        }

        fn transcript(&self, call_id: &str, utterance_id: &str, text: &str, is_final: bool) {
            self.say(AdapterMessage::Transcript(Transcript {
                call_id: call_id.into(),
                utterance_id: utterance_id.into(),
                text: text.into(),
                is_final,
                confidence: None,
                language: None,
            }));
        }

        fn error(&self, call_id: &str, code: ErrorCode) {
            self.say(AdapterMessage::Error(
                ErrorMessage::new(code, "test").for_call(call_id),
            ));
        }
    }

    /// A call on a hand-driven adapter with `capabilities`, and its `call_id`.
    async fn manual_call(capabilities: Capabilities) -> (Call, Manual, String) {
        let (adapters, mut started) = mpsc::unbounded_channel();
        let starter = Arc::new(ManualStarter {
            capabilities,
            adapters,
        });
        let call = start_call(starter).await;
        let mut adapter = started.recv().await.unwrap();
        let BridgeMessage::CallStarted(started) = adapter.next().await else {
            panic!("call_started first");
        };
        assert_eq!(started.peer, "Owner Name");
        assert_eq!(started.language, "en-US");
        (call, adapter, started.call_id)
    }

    fn capabilities() -> Capabilities {
        Capabilities {
            stt: true,
            tts: true,
            vad: true,
            barge_in: false,
            partials: true,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn partials_keep_listening_and_only_the_first_final_counts() {
        let (mut call, adapter, id) = manual_call(capabilities()).await;
        adapter.transcript(&id, "u1", "app", false);
        adapter.transcript(&id, "u1", "appro", false);
        adapter.transcript(&id, "u1", " approve ", true);
        adapter.transcript(&id, "u1", "approve again", true);
        adapter.transcript("other", "u2", "not ours", true);
        let seen = call.during(Duration::from_millis(500)).await;
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert_eq!(
            seen[..2],
            [
                SessionEvent::Listening(true),
                SessionEvent::Listening(false)
            ]
        );
        assert!(same(&seen[2], &transcript("approve")), "{seen:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn listening_ends_after_thirty_seconds_without_a_final() {
        let (mut call, adapter, id) = manual_call(capabilities()).await;
        adapter.speech_started(&id, "u1");
        assert_eq!(
            call.until(&SessionEvent::Listening(true)).await,
            vec![SessionEvent::Listening(true)]
        );
        let started = Instant::now();
        call.until(&SessionEvent::Listening(false)).await;
        assert!(started.elapsed() >= LISTENING_TIMEOUT - Duration::from_millis(50));
        assert!(started.elapsed() < LISTENING_TIMEOUT + Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_recognition_failure_is_not_understood() {
        let (mut call, adapter, id) = manual_call(capabilities()).await;
        adapter.speech_started(&id, "u1");
        adapter.error(&id, ErrorCode::RecognitionFailed);
        assert_eq!(
            call.until(&SessionEvent::SpeechFailed("recognition_failed".into()))
                .await,
            vec![
                SessionEvent::Listening(true),
                SessionEvent::Listening(false),
                SessionEvent::SpeechFailed("recognition_failed".into())
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn without_vad_a_final_transcript_stops_the_speech() {
        let mut no_vad = capabilities();
        no_vad.vad = false;
        let (mut call, mut adapter, id) = manual_call(no_vad).await;
        call.session.say_reply("a long reply");
        let BridgeMessage::Speak(speak) = adapter.next().await else {
            panic!("a speak");
        };
        assert_eq!(speak.speak_id, "s1");
        adapter.transcript(&id, "u1", "stop", true);
        let seen = call.until(&transcript("stop")).await;
        assert_eq!(seen[0], SessionEvent::BargedIn, "{seen:?}");
        assert!(matches!(
            adapter.next().await,
            BridgeMessage::StopSpeaking(StopSpeaking { speak_id: None, .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unavailable_engine_makes_the_call_chat_only() {
        let (mut call, mut adapter, id) = manual_call(capabilities()).await;
        call.owner.say(&tones::silence(10)).await;
        sleep(Duration::from_millis(100)).await;
        assert_eq!(adapter.audio.load(Ordering::Relaxed), 10);
        call.session.say_reply("one");
        assert!(matches!(adapter.next().await, BridgeMessage::Speak(_)));
        adapter.error(&id, ErrorCode::EngineUnavailable);
        assert_eq!(
            call.until(&SessionEvent::AdapterUnavailable).await,
            vec![SessionEvent::AdapterUnavailable]
        );
        assert!(matches!(
            adapter.next().await,
            BridgeMessage::StopSpeaking(_)
        ));
        call.owner.say(&tones::silence(10)).await;
        call.session.say("two");
        call.session.say_reply("three");
        assert!(!call.session.speaking());
        adapter.nothing().await;
        assert_eq!(adapter.audio.load(Ordering::Relaxed), 10, "no more audio");
    }

    #[tokio::test(start_paused = true)]
    async fn muting_stops_the_speech_and_sends_nothing_until_unmuted() {
        let (mut call, mut adapter, _id) = manual_call(capabilities()).await;
        call.session.say_reply("one");
        assert!(matches!(adapter.next().await, BridgeMessage::Speak(_)));
        call.session.set_silenced(true);
        assert!(matches!(
            adapter.next().await,
            BridgeMessage::StopSpeaking(_)
        ));
        call.session.say("two");
        adapter.nothing().await;
        call.session.set_silenced(false);
        call.session.say("three");
        let BridgeMessage::Speak(speak) = adapter.next().await else {
            panic!("a speak");
        };
        assert_eq!(
            (speak.speak_id.as_str(), speak.text.as_str()),
            ("s2", "three")
        );
        assert!(call.during(Duration::from_millis(100)).await.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn without_tts_nothing_is_spoken_and_long_text_is_split() {
        let mut deaf = capabilities();
        deaf.tts = false;
        let (call, mut adapter, _id) = manual_call(deaf).await;
        call.session.say("hello");
        assert!(!call.session.speaking());
        adapter.nothing().await;

        let (call, mut adapter, _id) = manual_call(capabilities()).await;
        call.session.say("é".repeat(2 * MAX_TEXT_CHARS + 10));
        let mut lengths = Vec::new();
        for _ in 0..3 {
            let BridgeMessage::Speak(speak) = adapter.next().await else {
                panic!("a speak");
            };
            assert!(!speak.interruptible);
            lengths.push(speak.text.chars().count());
        }
        assert_eq!(lengths, [MAX_TEXT_CHARS, MAX_TEXT_CHARS, 10]);
    }

    #[tokio::test(start_paused = true)]
    async fn audio_of_a_stopped_speak_is_dropped_until_its_speak_done() {
        let (mut call, mut adapter, id) = manual_call(capabilities()).await;
        call.session.say_reply("one");
        assert!(matches!(adapter.next().await, BridgeMessage::Speak(_)));
        adapter.sound(0);
        call.until(&SessionEvent::Speaking(true)).await;
        call.session.hush();
        call.until(&SessionEvent::Speaking(false)).await;
        assert!(matches!(
            adapter.next().await,
            BridgeMessage::StopSpeaking(_)
        ));
        call.session.say("two");
        let BridgeMessage::Speak(two) = adapter.next().await else {
            panic!("a speak");
        };
        assert_eq!(two.speak_id, "s2");
        for seq in 1..4 {
            adapter.sound(seq);
        }
        assert!(call.during(Duration::from_millis(200)).await.is_empty());
        assert!(call.owner.heard_since_clear().is_empty(), "s1's late audio");
        adapter.speak_done(&id, "s1", true);
        adapter.sound(4);
        adapter.sound(5);
        call.until(&SessionEvent::Speaking(true)).await;
        adapter.speak_done(&id, "s2", false);
        call.until(&SessionEvent::Speaking(false)).await;
        assert_eq!(call.owner.heard_since_clear().len(), 2 * SAMPLES_PER_FRAME);
        assert_eq!(
            call.setup.metrics.snapshot(),
            Metrics::default(),
            "until the end"
        );
    }

    #[test]
    fn the_peers_name_is_cut_to_the_protocols_cap() {
        assert_eq!(display_name("Ann\tLee"), "Ann Lee");
        let long = "ü".repeat(100);
        assert_eq!(display_name(&long).chars().count(), MAX_NAME_CHARS);
    }
}
