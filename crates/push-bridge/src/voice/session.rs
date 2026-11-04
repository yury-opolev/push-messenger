//! One call's pipeline: the owner's audio in, transcripts out; sentences in, the agent's
//! audio out. The speech itself runs in a [`SpeechEngine`]: the voice adapter's
//! ([`Session::with_adapter`], [`AdapterEngine`]).
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
//! - The engine reports each utterance's text, in order: a non-empty text is a
//!   [`SessionEvent::Transcript`] (stamped with when its speech started), a failure a
//!   [`SessionEvent::SpeechFailed`] with a content-free code.
//! - The engine speaks the queued sentences one by one and queues their frames on the
//!   line's speaker, which plays them in real time: it reports `Speaking(false)` only once
//!   the queued audio played.
//!   Sentences queued before a barge-in or a [`Session::hush`] are never spoken.
//!
//! Logs carry durations, counts and codes only: never audio or text.
//!
//! [`AdapterEngine`]: super::adapter::router::AdapterEngine

use std::sync::Arc;
use std::sync::atomic::Ordering;

use push_sdk::{CallEvent, CallId};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::Instant;

use super::Frame;
use super::adapter::router::{AdapterEngine, AdapterSetup};
use super::engine::{Sentence, SpeechContext, SpeechEngine};
use super::phone::{Line, LineControl, LineEnded};

/// What a call's pipeline reports to the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Audio flows.
    Connected,
    /// The owner started (true) or stopped (false) speaking.
    Listening(bool),
    /// The owner spoke over the agent: its speech was cut.
    BargedIn,
    /// An utterance's text (never empty), and when its speech started (the adapter's
    /// `speech_started`).
    Transcript { text: String, started: Instant },
    /// An utterance could not be transcribed (content-free code: `recognition_failed`
    /// from the voice adapter).
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

    /// Queues one sentence to speak (ignored when the adapter cannot speak), muted or not: for what the
    /// bridge itself says (the greeting, cards, confirmations).
    pub fn say(&self, text: impl Into<String>) {
        self.queue(text.into(), false);
    }

    /// Queues one sentence of the agent's reply to speak, unless a barge-in muted the
    /// reply (ignored when the adapter cannot speak).
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

    /// The bridge stops: the engine ends the call as a shutdown (the voice adapter hears
    /// `call_ended {reason: shutdown}`; at most 2 s), then the session is dropped, which
    /// hangs up.
    pub async fn shut_down(self) {
        let wait = std::time::Duration::from_secs(2);
        let _ = tokio::time::timeout(wait, self.engine.shut_down()).await;
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
            // The engine first: a greeting said at `Connected` finds it ready.
            engine.connected();
            ctx.emit(SessionEvent::Connected);
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
    use std::time::Duration;

    use push_sdk::CallEndReason;
    use push_voice_adapter_proto::BridgeHello;
    use push_voice_adapter_testkit::{Script, Word, tones};

    use super::*;
    use crate::voice::FRAME;
    use crate::voice::adapter::supervisor::Supervisor;
    use crate::voice::adapter::supervisor::testing::{Plan, ScriptedStarter};
    use crate::voice::phone::{FakeCall, FakePhone, Phone};

    struct Call {
        session: Session,
        owner: FakeCall,
        events: mpsc::UnboundedReceiver<SessionEvent>,
        /// Keeps the adapter's supervisor running.
        _setup: AdapterSetup,
    }

    const WORD_HZ: f64 = 450.0;

    /// The owner says a word: a tone between silences.
    fn word() -> Vec<i16> {
        let mut audio = tones::silence(5);
        audio.extend(tones::tone(WORD_HZ, 20));
        audio.extend(tones::silence(25));
        audio
    }

    /// The testkit's adapter, hearing [`word`], once ready.
    async fn adapter() -> AdapterSetup {
        let script = Script::with_words(vec![Word::new(WORD_HZ, "hello")]);
        let starter = ScriptedStarter::new([Plan::Adapter(script)]);
        let supervisor = Supervisor::with_starter(starter, BridgeHello::v1("1.2.3", "en-US"));
        let mut current = supervisor.current();
        tokio::time::timeout(Duration::from_secs(10), current.wait_for(Option::is_some))
            .await
            .expect("the adapter gets ready")
            .unwrap();
        AdapterSetup::new(supervisor, "en-US")
    }

    fn sink() -> (EventSink, mpsc::UnboundedReceiver<SessionEvent>) {
        let (sender, events) = mpsc::unbounded_channel();
        let sink: EventSink = Arc::new(move |event| {
            let _ = sender.send(event);
        });
        (sink, events)
    }

    /// A connected call whose speech runs in the testkit's adapter.
    async fn call() -> Call {
        let setup = adapter().await;
        let phone = FakePhone::new();
        let line = phone.answer(CallId::new(), "offer").await.unwrap();
        let owner = phone.wait_answered().await;
        let (sink, events) = sink();
        let session = Session::with_adapter(line, &setup, "", sink);
        let mut call = Call {
            session,
            owner,
            events,
            _setup: setup,
        };
        call.until(&SessionEvent::Connected).await;
        call
    }

    impl Call {
        /// The events up to and including `wanted`; panics after 60 s.
        async fn until(&mut self, wanted: &SessionEvent) -> Vec<SessionEvent> {
            let mut seen = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                match tokio::time::timeout_at(deadline, self.events.recv()).await {
                    Ok(Some(event)) => {
                        let found = event == *wanted;
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

    fn words(count: usize) -> String {
        vec!["word"; count].join(" ")
    }

    /// Frames the testkit's voice speaks `text` for (40 ms a character).
    fn frames_of(text: &str) -> usize {
        Script::default().speak_frames(text.chars().count())
    }

    #[tokio::test(start_paused = true)]
    async fn a_barge_in_mutes_the_reply_until_unmuted() {
        let mut call = call().await;
        call.session.say_reply(words(50));
        call.until(&SessionEvent::Speaking(true)).await;
        let owner = call.owner.clone();
        let speaking = tokio::spawn(async move { owner.say_paced(&word()).await });
        call.until(&SessionEvent::BargedIn).await;
        call.until(&SessionEvent::Listening(false)).await;
        speaking.await.unwrap();
        let seen = call.during(Duration::from_millis(300)).await;
        assert!(
            matches!(&seen[..], [SessionEvent::Transcript { text, .. }] if text == "hello"),
            "{seen:?}"
        );
        // The core has not handled `BargedIn` yet: the reply's next sentence is dropped.
        call.session.say_reply("the rest of the reply");
        assert!(!call.session.speaking());
        assert!(call.during(Duration::from_millis(300)).await.is_empty());
        assert!(call.owner.heard_since_clear().is_empty());

        call.session.say("one two");
        call.until(&SessionEvent::Speaking(true)).await;
        call.until(&SessionEvent::Speaking(false)).await;

        assert!(call.session.unmute());
        assert!(!call.session.unmute());
        call.session.say_reply("one two three four five");
        call.until(&SessionEvent::Speaking(true)).await;
        call.until(&SessionEvent::Speaking(false)).await;
        let spoken = frames_of("one two") + frames_of("one two three four five");
        call.owner
            .wait_until("the prompt and the reply", |owner| {
                owner.heard_since_clear().len() >= spoken * FRAME
            })
            .await;
        assert_eq!(call.owner.heard_since_clear().len(), spoken * FRAME);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hush_does_not_mute_the_reply() {
        let mut call = call().await;
        call.session.say_reply(words(50));
        call.until(&SessionEvent::Speaking(true)).await;
        call.session.hush();
        call.until(&SessionEvent::Speaking(false)).await;
        assert!(!call.session.unmute());
        call.session.say_reply("one two");
        call.until(&SessionEvent::Speaking(true)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn hush_drops_the_queue_without_a_barge_in() {
        let mut call = call().await;
        call.session.say_reply(words(50));
        call.session.say_reply(words(50));
        call.until(&SessionEvent::Speaking(true)).await;
        call.session.hush();
        assert!(!call.session.speaking());
        let seen = call.during(Duration::from_millis(500)).await;
        assert_eq!(seen, vec![SessionEvent::Speaking(false)]);
        assert!(call.owner.clears() >= 1);
        assert!(call.owner.heard().len() < 50 * FRAME);
        call.session.hush();
        assert!(call.during(Duration::from_millis(100)).await.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_owner_hanging_up_ends_the_session_once() {
        let mut call = call().await;
        call.session.say_reply(words(50));
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

    #[tokio::test(start_paused = true)]
    async fn the_call_ending_mid_speech_stops_the_speech_first() {
        let mut call = call().await;
        call.session.say_reply(words(50));
        call.session.say_reply(words(50));
        call.until(&SessionEvent::Speaking(true)).await;
        call.owner.end(CallEndReason::Completed);
        let ended = SessionEvent::Ended(LineEnded {
            reason: CallEndReason::Completed,
            duration_s: Some(0),
        });
        let seen = call.until(&ended).await;
        assert_eq!(
            seen[seen.len() - 2..],
            [SessionEvent::Speaking(false), ended],
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

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
    async fn dropping_the_session_hangs_up() {
        let call = call().await;
        let owner = call.owner.clone();
        drop(call);
        owner
            .wait_until("the hang-up", |owner| owner.hung_up())
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_ending_before_it_connects_ends_without_connecting() {
        let setup = adapter().await;
        let phone = FakePhone::new();
        phone.delay_connect(Duration::from_secs(5));
        let line = phone.answer(CallId::new(), "offer").await.unwrap();
        let owner = phone.wait_answered().await;
        let (sink, mut events) = sink();
        let session = Session::with_adapter(line, &setup, "", sink);
        let id = session.call_id();
        let _setup = setup;
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
