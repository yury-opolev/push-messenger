//! One call's pipeline: the owner's audio in, transcripts out; sentences in, the agent's
//! audio out. The speech itself runs in a [`SpeechEngine`]: the bridge's own providers
//! ([`Session::start`], [`BuiltinEngine`]) or a voice adapter ([`Session::with_adapter`],
//! [`AdapterEngine`]).
//!
//! - A watcher waits for the media to connect ([`SessionEvent::Connected`]) and for the
//!   end: [`SessionEvent::Ended`] exactly once, after which every task of the session has
//!   stopped (the owner's audio closing too does not report a second end).
//! - The listener gives the owner's audio to the engine. When speech starts while the
//!   agent speaks, that is a barge-in: the queued sentences and the queued audio are
//!   dropped ([`SessionEvent::BargedIn`], then `Speaking(false)`); then
//!   `Listening(true)`. An utterance (or a discarded noise) is `Listening(false)`.
//!   A barge-in also mutes the agent's reply: [`Session::say_reply`] drops its sentences
//!   until [`Session::unmute`], so the rest of the interrupted reply is not spoken even
//!   when the core handles reply text before it handles the `BargedIn` event.
//! - The engine transcribes one utterance at a time, in order: a non-empty text is a
//!   [`SessionEvent::Transcript`] (stamped with when its speech started), a failure a
//!   [`SessionEvent::SpeechFailed`] with a content-free code.
//! - The engine speaks the queued sentences one by one and queues their frames on the
//!   line's speaker, which plays them in real time (and may queue seconds ahead): it keeps
//!   when the queued audio will have played and reports `Speaking(false)` only then.
//!   Sentences queued before a barge-in or a [`Session::hush`] are never spoken.
//!
//! Logs carry durations, counts and codes only: never audio or text.
//!
//! [`BuiltinEngine`]: super::engine::BuiltinEngine
//! [`AdapterEngine`]: super::adapter::router::AdapterEngine

use std::sync::Arc;
use std::sync::atomic::Ordering;

use push_sdk::{CallEvent, CallId};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::Instant;

use super::Frame;
use super::adapter::router::{AdapterEngine, AdapterSetup};
use super::engine::{BuiltinEngine, Sentence, SpeechContext, SpeechEngine};
use super::phone::{Line, LineControl, LineEnded};
use super::provider::{Stt, Tts};
use super::vad::VadConfig;

#[cfg(test)]
use super::phone::Speaker;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use tokio::sync::Notify;

/// What a call's pipeline reports to the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Audio flows.
    Connected,
    /// The owner started (true) or stopped (false) speaking.
    Listening(bool),
    /// The owner spoke over the agent: its speech was cut.
    BargedIn,
    /// An utterance's text (never empty), and when its speech started (the detector's
    /// onset, not its pre-roll; the adapter's `speech_started`).
    Transcript { text: String, started: Instant },
    /// An utterance could not be transcribed (content-free code: the STT provider's, or
    /// `recognition_failed` from the voice adapter).
    SpeechFailed(String),
    /// The agent started (true) or stopped (false) speaking.
    Speaking(bool),
    /// The voice adapter went away during the call (or none was ready when it connected):
    /// the call stays up without speech (chat-only) until [`SessionEvent::AdapterBack`].
    AdapterLost,
    /// A voice adapter is ready again after [`SessionEvent::AdapterLost`]: the call has
    /// speech again.
    AdapterBack,
    /// The voice adapter cannot recognise or synthesise right now
    /// (`engine_unavailable`): the call is chat-only until it ends (or a new adapter
    /// connects, [`SessionEvent::AdapterBack`] after a loss).
    AdapterUnavailable,
    /// The call is over.
    Ended(LineEnded),
}

/// Where a session reports its events.
///
/// It is called from the session's tasks and from [`Session::hush`], sometimes while
/// the session holds its speech lock (so that `Speaking` events stay in order with
/// barge-ins and hushes): it must not block and must never call back into the
/// [`Session`] (the core's sink only sends into a channel).
pub type EventSink = Arc<dyn Fn(SessionEvent) + Send + Sync>;

/// A running call's pipeline. Dropping it hangs up a call still running and stops its
/// tasks.
pub struct Session {
    call_id: CallId,
    ctx: Arc<SpeechContext>,
    engine: Arc<dyn SpeechEngine>,
    control: Arc<dyn LineControl>,
    tasks: Vec<AbortHandle>,
}

impl Session {
    /// Runs the call's pipeline with the bridge's own speech providers on the current
    /// runtime until it ends.
    pub fn start(
        line: Line,
        stt: Arc<dyn Stt>,
        tts: Option<Arc<dyn Tts>>,
        vad: VadConfig,
        events: EventSink,
    ) -> Self {
        Self::with_engine(line, events, move |ctx| {
            Arc::new(BuiltinEngine::start(ctx, stt, tts, vad))
        })
    }

    /// Runs the call's pipeline with speech in the voice adapter `adapter` supervises.
    /// `peer`: the owner's display name, for the adapter (cut to 64 characters).
    pub fn with_adapter(line: Line, adapter: &AdapterSetup, peer: &str, events: EventSink) -> Self {
        Self::with_engine(line, events, |ctx| {
            Arc::new(AdapterEngine::start(ctx, adapter, peer))
        })
    }

    fn with_engine(
        line: Line,
        events: EventSink,
        engine: impl FnOnce(Arc<SpeechContext>) -> Arc<dyn SpeechEngine>,
    ) -> Self {
        let Line {
            call_id,
            incoming,
            speaker,
            control,
        } = line;
        let ctx = Arc::new(SpeechContext::new(events, speaker));
        let engine = engine(ctx.clone());
        let listener = tokio::spawn(listen(engine.clone(), incoming));
        let mut tasks = vec![listener.abort_handle()];
        let watcher = tokio::spawn(watch(
            ctx.clone(),
            control.clone(),
            engine.clone(),
            listener,
        ));
        tasks.push(watcher.abort_handle());
        Self {
            call_id,
            ctx,
            engine,
            control,
            tasks,
        }
    }

    pub fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Queues one sentence to speak (ignored without TTS), muted or not: for what the
    /// bridge itself says (the greeting, cards, confirmations).
    pub fn say(&self, text: impl Into<String>) {
        self.queue(text.into(), false);
    }

    /// Queues one sentence of the agent's reply to speak, unless a barge-in muted the
    /// reply (ignored without TTS).
    pub fn say_reply(&self, text: impl Into<String>) {
        self.queue(text.into(), true);
    }

    /// The owner is answered again: the next replies are spoken. Whether a barge-in had
    /// muted them.
    pub fn unmute(&self) -> bool {
        std::mem::take(&mut self.ctx.speech().muted)
    }

    fn queue(&self, text: String, reply: bool) {
        if !self.engine.speaks() || self.ctx.silenced.load(Ordering::Acquire) {
            return;
        }
        {
            // Under the lock: the watcher resets the speech state under it once ended,
            // and a barge-in mutes the reply under it.
            let mut speech = self.ctx.speech();
            if self.ctx.ended() || (reply && speech.muted) {
                return;
            }
            let generation = self.ctx.generation();
            speech.queue.push_back(Sentence {
                generation,
                text,
                own: !reply,
            });
        }
        self.ctx.wake.notify_one();
    }

    /// Stops speaking: drops the queued sentences and the queued audio (nothing once the
    /// call ended). Unlike a barge-in it does not mute the reply: who hushes decides what
    /// is said next.
    pub fn hush(&self) {
        self.ctx.stop_speaking(false);
        self.engine.stop();
    }

    /// Mutes the agent (`true`: stops speaking at once and says nothing more, its own
    /// sentences included) or lets it speak again; the owner is still heard.
    pub fn set_silenced(&self, silenced: bool) {
        self.ctx.silenced.store(silenced, Ordering::Release);
        if silenced {
            self.ctx.stop_speaking(false);
            self.engine.stop();
        }
    }

    /// Forwards a call event to the line; `true` when it was about this call.
    pub fn handle(&self, event: &CallEvent) -> bool {
        self.control.handle(event)
    }

    /// Hangs up (completed); `Ended` follows.
    pub fn hang_up(&self) {
        let control = self.control.clone();
        tokio::spawn(async move {
            control.hangup().await;
        });
    }

    /// Whether the agent is speaking (queued sentences or audio still playing).
    pub fn speaking(&self) -> bool {
        self.ctx.speaking()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        if !self.ctx.ended() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let control = self.control.clone();
                runtime.spawn(async move {
                    control.hangup().await;
                });
            }
        }
    }
}

/// Reports the connection and the end, then stops the listener and the engine.
async fn watch(
    ctx: Arc<SpeechContext>,
    control: Arc<dyn LineControl>,
    engine: Arc<dyn SpeechEngine>,
    listener: JoinHandle<()>,
) {
    let end = match control.connected().await {
        Ok(()) => {
            ctx.emit(SessionEvent::Connected);
            engine.connected();
            control.ended().await
        }
        Err(end) => end,
    };
    ctx.set_ended();
    listener.abort();
    let _ = listener.await;
    engine.end(end.reason).await;
    {
        // The stopped engine may have left a sentence busy, sentences queued or
        // `Speaking(true)` unanswered: the end stops the speech too.
        let mut speech = ctx.speech();
        speech.queue.clear();
        speech.busy = None;
        speech.playing_until = None;
        if std::mem::take(&mut speech.speaking) {
            ctx.emit(SessionEvent::Speaking(false));
        }
    }
    tracing::info!(
        event = "voice_call_ended",
        reason = end.reason.as_str(),
        duration_s = end.duration_s
    );
    ctx.emit(SessionEvent::Ended(end));
}

/// Gives the owner's audio to the engine.
async fn listen(engine: Arc<dyn SpeechEngine>, mut incoming: mpsc::Receiver<Frame>) {
    while let Some(frame) = incoming.recv().await {
        engine.hear(&frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::fake::{FakeStt, FakeTts, TTS_HZ, amplitude, silence, tone};
    use crate::voice::phone::{FakeCall, FakePhone, Phone};
    use crate::voice::provider::SpeechError;
    use push_sdk::CallEndReason;

    const SAMPLES_PER_MS: usize = 16;

    fn stt() -> Arc<FakeStt> {
        Arc::new(FakeStt::new(vec![
            (450.0, "hello".into()),
            (900.0, "approve".into()),
        ]))
    }

    struct Call {
        session: Session,
        owner: FakeCall,
        events: mpsc::UnboundedReceiver<SessionEvent>,
    }

    async fn call_with(stt: Arc<dyn Stt>, tts: Option<Arc<dyn Tts>>) -> Call {
        let phone = FakePhone::new();
        let line = phone.answer(CallId::new(), "offer").await.unwrap();
        let owner = phone.wait_answered().await;
        let (sender, events) = mpsc::unbounded_channel();
        let sink: EventSink = Arc::new(move |event| {
            let _ = sender.send(event);
        });
        let session = Session::start(line, stt, tts, VadConfig::default(), sink);
        let mut call = Call {
            session,
            owner,
            events,
        };
        call.until(&SessionEvent::Connected).await;
        call
    }

    async fn call() -> Call {
        call_with(stt(), Some(Arc::new(FakeTts::new()))).await
    }

    impl Call {
        /// The events up to and including `wanted`; panics after 10 s.
        async fn until(&mut self, wanted: &SessionEvent) -> Vec<SessionEvent> {
            let mut seen = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(10);
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

    /// A transcript of `text`; [`same`] does not compare its onset.
    fn transcript(text: &str) -> SessionEvent {
        SessionEvent::Transcript {
            text: text.into(),
            started: Instant::now(),
        }
    }

    /// Whether `event` is `wanted`, but for a transcript's onset.
    fn same(event: &SessionEvent, wanted: &SessionEvent) -> bool {
        match (event, wanted) {
            (
                SessionEvent::Transcript { text, .. },
                SessionEvent::Transcript { text: wanted, .. },
            ) => text == wanted,
            _ => event == wanted,
        }
    }

    fn words(count: usize) -> String {
        vec!["word"; count].join(" ")
    }

    fn utterance(hz: f64) -> Vec<i16> {
        let mut audio = silence(200);
        audio.extend(tone(hz, 900, 8000.0));
        audio.extend(silence(900));
        audio
    }

    #[tokio::test]
    async fn an_utterance_becomes_a_transcript() {
        let mut call = call().await;
        let before = Instant::now();
        call.owner.say_paced(&utterance(900.0)).await;
        let said = Instant::now();
        let seen = call.until(&transcript("approve")).await;
        assert_eq!(
            seen[..2],
            [
                SessionEvent::Listening(true),
                SessionEvent::Listening(false)
            ]
        );
        assert_eq!(seen.len(), 3);
        let SessionEvent::Transcript { text, started } = &seen[2] else {
            panic!("{seen:?}");
        };
        assert_eq!(text, "approve");
        // The onset: after the 200 ms of silence, long before the utterance ended.
        assert!(*started >= before + Duration::from_millis(200), "{seen:?}");
        assert!(*started + Duration::from_millis(500) < said, "{seen:?}");
        assert!(!call.session.speaking());
    }

    #[tokio::test]
    async fn a_sentence_is_spoken_and_speaking_ends_after_playback() {
        let mut call = call().await;
        let started = Instant::now();
        call.session.say("one two three four five");
        assert!(call.session.speaking());
        assert_eq!(
            call.until(&SessionEvent::Speaking(true)).await,
            vec![SessionEvent::Speaking(true)]
        );
        assert!(call.session.speaking());
        assert_eq!(
            call.until(&SessionEvent::Speaking(false)).await,
            vec![SessionEvent::Speaking(false)]
        );
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(2000), "{elapsed:?}");
        assert!(!call.session.speaking());
        call.owner
            .wait_until("the whole sentence", |owner| {
                owner.heard().len() >= 300 * SAMPLES_PER_MS
            })
            .await;
        let heard = call.owner.heard();
        assert_eq!(heard.len(), 300 * SAMPLES_PER_MS);
        assert!(amplitude(&heard, TTS_HZ) > 5000.0);
    }

    #[tokio::test]
    async fn the_owner_barges_in_and_the_agent_stops_at_once() {
        let mut call = call().await;
        call.session.say(words(50));
        call.session.say(words(20));
        call.until(&SessionEvent::Speaking(true)).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let owner = call.owner.clone();
        let speaking = tokio::spawn(async move { owner.say_paced(&utterance(450.0)).await });
        let seen = call.until(&SessionEvent::BargedIn).await;
        assert!(!seen.contains(&SessionEvent::Speaking(false)), "{seen:?}");
        assert_eq!(
            call.until(&SessionEvent::Listening(true)).await,
            vec![SessionEvent::Speaking(false), SessionEvent::Listening(true)]
        );
        assert!(call.owner.clears() >= 1);
        assert!(!call.session.speaking());
        let heard = call.owner.heard().len();
        assert!(heard < 1500 * SAMPLES_PER_MS, "heard {heard} samples");
        let seen = call.until(&transcript("hello")).await;
        assert!(!seen.contains(&SessionEvent::Speaking(true)), "{seen:?}");
        speaking.await.unwrap();
        assert_eq!(
            call.owner.heard().len(),
            heard,
            "nothing after the barge-in"
        );

        call.session.say("one two");
        call.until(&SessionEvent::Speaking(true)).await;
        call.until(&SessionEvent::Speaking(false)).await;
        call.owner
            .wait_until("the sentence after the barge-in", |owner| {
                owner.heard_since_clear().len() >= 200 * SAMPLES_PER_MS
            })
            .await;
        let after = call.owner.heard_since_clear();
        assert_eq!(after.len(), 200 * SAMPLES_PER_MS);
        assert!(amplitude(&after, TTS_HZ) > 5000.0);
    }

    #[tokio::test]
    async fn a_barge_in_mutes_the_reply_until_unmuted() {
        let mut call = call().await;
        call.session.say(words(50));
        call.until(&SessionEvent::Speaking(true)).await;
        call.owner.say(&utterance(450.0)).await;
        call.until(&SessionEvent::BargedIn).await;
        call.until(&transcript("hello")).await;
        // The core has not handled `BargedIn` yet: the reply's next sentence is dropped.
        call.session.say_reply("the rest of the reply");
        assert!(!call.session.speaking());
        assert!(call.during(Duration::from_millis(300)).await.is_empty());
        assert!(call.owner.heard_since_clear().is_empty());

        call.session.say("one two");
        call.until(&SessionEvent::Speaking(true)).await;
        call.until(&SessionEvent::Speaking(false)).await;
        call.owner
            .wait_until("the prompt", |owner| {
                owner.heard_since_clear().len() >= 200 * SAMPLES_PER_MS
            })
            .await;

        assert!(call.session.unmute());
        assert!(!call.session.unmute());
        call.session.say_reply("one two three four five");
        call.until(&SessionEvent::Speaking(true)).await;
        call.until(&SessionEvent::Speaking(false)).await;
        call.owner
            .wait_until("the reply", |owner| {
                owner.heard_since_clear().len() >= 500 * SAMPLES_PER_MS
            })
            .await;
        assert_eq!(call.owner.heard_since_clear().len(), 500 * SAMPLES_PER_MS);
    }

    #[tokio::test]
    async fn a_hush_does_not_mute_the_reply() {
        let mut call = call().await;
        call.session.say(words(50));
        call.until(&SessionEvent::Speaking(true)).await;
        call.session.hush();
        call.until(&SessionEvent::Speaking(false)).await;
        assert!(!call.session.unmute());
        call.session.say_reply("one two");
        call.until(&SessionEvent::Speaking(true)).await;
    }

    #[tokio::test]
    async fn hush_drops_the_queue_without_a_barge_in() {
        let mut call = call().await;
        call.session.say(words(50));
        call.session.say(words(50));
        call.until(&SessionEvent::Speaking(true)).await;
        call.session.hush();
        assert!(!call.session.speaking());
        let seen = call.during(Duration::from_millis(500)).await;
        assert_eq!(seen, vec![SessionEvent::Speaking(false)]);
        assert!(call.owner.clears() >= 1);
        assert!(call.owner.heard().len() < 1000 * SAMPLES_PER_MS);
        call.session.hush();
        assert!(call.during(Duration::from_millis(100)).await.is_empty());
    }

    #[tokio::test]
    async fn the_owner_hanging_up_ends_the_session_once() {
        let mut call = call().await;
        call.session.say(words(50));
        call.until(&SessionEvent::Speaking(true)).await;
        call.owner.end(CallEndReason::Completed);
        let end = LineEnded {
            reason: CallEndReason::Completed,
            duration_s: Some(0),
        };
        assert_eq!(call.owner.ended(), Some(end));
        let seen = call.until(&SessionEvent::Ended(end)).await;
        assert!(!seen.contains(&SessionEvent::Connected), "{seen:?}");
        assert!(call.during(Duration::from_millis(300)).await.is_empty());
        assert!(!call.owner.hung_up());
        call.session.say("too late");
        assert!(call.during(Duration::from_millis(100)).await.is_empty());
    }

    #[tokio::test]
    async fn the_call_ending_mid_speech_stops_the_speech_first() {
        let mut call = call().await;
        call.session.say(words(50));
        call.session.say(words(50));
        call.until(&SessionEvent::Speaking(true)).await;
        call.owner.end(CallEndReason::Completed);
        let seen = call
            .until(&SessionEvent::Ended(LineEnded {
                reason: CallEndReason::Completed,
                duration_s: Some(0),
            }))
            .await;
        assert_eq!(
            seen[seen.len() - 2..],
            [
                SessionEvent::Speaking(false),
                SessionEvent::Ended(LineEnded {
                    reason: CallEndReason::Completed,
                    duration_s: Some(0),
                }),
            ],
            "{seen:?}"
        );
        assert_eq!(
            seen.iter()
                .filter(|e| **e == SessionEvent::Speaking(false))
                .count(),
            1,
            "{seen:?}"
        );
        assert!(!call.session.speaking());
        call.session.hush();
        call.session.say("too late");
        assert!(!call.session.speaking());
        assert!(call.during(Duration::from_millis(200)).await.is_empty());
    }

    /// The line's speaker, holding its first frame until released.
    struct HeldSpeaker {
        inner: Arc<dyn Speaker>,
        held: AtomicBool,
        taking: Notify,
        release: Notify,
    }

    #[async_trait::async_trait]
    impl Speaker for HeldSpeaker {
        async fn send(&self, frame: Frame) -> bool {
            if !self.held.swap(true, Ordering::AcqRel) {
                self.taking.notify_one();
                self.release.notified().await;
            }
            self.inner.send(frame).await
        }

        fn clear(&self) {
            self.inner.clear();
        }
    }

    #[tokio::test]
    async fn a_frame_overtaken_by_a_hush_does_not_play() {
        let phone = FakePhone::new();
        let mut line = phone.answer(CallId::new(), "offer").await.unwrap();
        let owner = phone.wait_answered().await;
        let held = Arc::new(HeldSpeaker {
            inner: line.speaker.clone(),
            held: AtomicBool::new(false),
            taking: Notify::new(),
            release: Notify::new(),
        });
        line.speaker = held.clone();
        let (sender, events) = mpsc::unbounded_channel();
        let sink: EventSink = Arc::new(move |event| {
            let _ = sender.send(event);
        });
        let session = Session::start(
            line,
            stt(),
            Some(Arc::new(FakeTts::new())),
            VadConfig::default(),
            sink,
        );
        let mut call = Call {
            session,
            owner,
            events,
        };
        call.until(&SessionEvent::Connected).await;
        call.session.say("one two");
        held.taking.notified().await;
        call.session.hush();
        assert_eq!(call.owner.clears(), 1);
        held.release.notify_one();
        call.owner
            .wait_until("the second clear", |owner| owner.clears() == 2)
            .await;
        assert_eq!(
            call.during(Duration::from_millis(300)).await,
            vec![SessionEvent::Speaking(true), SessionEvent::Speaking(false)]
        );
        assert!(call.owner.heard().is_empty());
        assert!(!call.session.speaking());
    }

    #[tokio::test]
    async fn hang_up_hangs_up_and_ends() {
        let mut call = call().await;
        call.session.hang_up();
        let seen = call.during(Duration::from_millis(500)).await;
        assert_eq!(seen.len(), 1, "{seen:?}");
        let SessionEvent::Ended(end) = &seen[0] else {
            panic!("{seen:?}");
        };
        assert_eq!(end.reason, CallEndReason::Completed);
        assert!(call.owner.hung_up());
    }

    #[tokio::test]
    async fn dropping_the_session_hangs_up() {
        let call = call().await;
        let owner = call.owner.clone();
        drop(call);
        owner
            .wait_until("the hang-up", |owner| owner.hung_up())
            .await;
    }

    #[tokio::test]
    async fn without_tts_nothing_is_spoken() {
        let mut call = call_with(stt(), None).await;
        call.session.say("one two three");
        assert!(!call.session.speaking());
        assert!(call.during(Duration::from_millis(300)).await.is_empty());
        assert!(call.owner.heard().is_empty());
        call.owner.say(&utterance(450.0)).await;
        call.until(&transcript("hello")).await;
    }

    struct FailingStt;

    #[async_trait::async_trait]
    impl Stt for FailingStt {
        fn name(&self) -> &'static str {
            "failing"
        }

        async fn transcribe(&self, _samples: &[i16]) -> Result<String, SpeechError> {
            Err(SpeechError::new("failing", "http_500"))
        }
    }

    #[tokio::test]
    async fn an_stt_failure_is_reported_with_its_code() {
        let mut call = call_with(Arc::new(FailingStt), None).await;
        call.owner.say(&utterance(450.0)).await;
        let seen = call
            .until(&SessionEvent::SpeechFailed("http_500".into()))
            .await;
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, SessionEvent::Transcript { .. }))
        );
    }

    #[tokio::test]
    async fn silence_and_the_unknown_make_no_transcript() {
        let mut call = call().await;
        call.owner.say(&utterance(1500.0)).await;
        call.until(&SessionEvent::Listening(false)).await;
        assert!(call.during(Duration::from_millis(300)).await.is_empty());
    }

    #[tokio::test]
    async fn a_call_ending_before_it_connects_ends_without_connecting() {
        let phone = FakePhone::new();
        phone.delay_connect(Duration::from_secs(5));
        let line = phone.answer(CallId::new(), "offer").await.unwrap();
        let owner = phone.wait_answered().await;
        let (sender, mut events) = mpsc::unbounded_channel();
        let sink: EventSink = Arc::new(move |event| {
            let _ = sender.send(event);
        });
        let session = Session::start(line, stt(), None, VadConfig::default(), sink);
        let id = session.call_id();
        assert!(session.handle(&CallEvent::Ended {
            call_id: id,
            reason: CallEndReason::Missed,
            duration_s: None,
        }));
        assert_eq!(
            events.recv().await,
            Some(SessionEvent::Ended(LineEnded {
                reason: CallEndReason::Missed,
                duration_s: None
            }))
        );
        assert_eq!(owner.handled().len(), 1);
    }
}
