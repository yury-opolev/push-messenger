//! One call's pipeline: the owner's audio in, transcripts out; sentences in, the agent's
//! audio out.
//!
//! - A watcher waits for the media to connect ([`SessionEvent::Connected`]) and for the
//!   end: [`SessionEvent::Ended`] exactly once, after which every task of the session has
//!   stopped (the owner's audio closing too does not report a second end).
//! - The listener runs the [`Vad`] on the owner's audio. When speech starts while the
//!   agent speaks, that is a barge-in: the queued sentences and the queued audio are
//!   dropped ([`SessionEvent::BargedIn`], then `Speaking(false)`); then
//!   `Listening(true)`. An utterance (or a discarded noise) is `Listening(false)`.
//! - The STT worker transcribes one utterance at a time, in order: a non-empty text is a
//!   [`SessionEvent::Transcript`], a failure a [`SessionEvent::SpeechFailed`] with the
//!   provider's content-free code.
//! - The speaker worker synthesizes the queued sentences one by one and queues their
//!   frames on the line's speaker, which plays them in real time (and may queue seconds
//!   ahead): it keeps when the queued audio will have played and reports
//!   `Speaking(false)` only then. Sentences queued before a barge-in or a
//!   [`Session::hush`] are never spoken.
//!
//! Logs carry durations, counts and codes only: never audio or text.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use push_sdk::{CallEvent, CallId};
use tokio::sync::{Notify, mpsc};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::Instant;

use super::phone::{Line, LineControl, LineEnded, Speaker};
use super::provider::{Stt, Tts};
use super::vad::{Vad, VadConfig, VadEvent};
use super::{Frame, SAMPLE_RATE, wav};

/// What a call's pipeline reports to the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Audio flows.
    Connected,
    /// The owner started (true) or stopped (false) speaking.
    Listening(bool),
    /// The owner spoke over the agent: its speech was cut.
    BargedIn,
    /// An utterance's text (never empty).
    Transcript(String),
    /// The STT provider failed on an utterance (content-free code).
    SpeechFailed(String),
    /// The agent started (true) or stopped (false) speaking.
    Speaking(bool),
    /// The call is over.
    Ended(LineEnded),
}

/// Where a session reports its events (called from its tasks: keep it quick).
pub type EventSink = Arc<dyn Fn(SessionEvent) + Send + Sync>;

const FRAME_TIME: Duration = Duration::from_millis(20);

/// A running call's pipeline. Dropping it hangs up a call still running and stops its
/// tasks.
pub struct Session {
    call_id: CallId,
    shared: Arc<Shared>,
    control: Arc<dyn LineControl>,
    tasks: Vec<AbortHandle>,
}

/// What the session's tasks share.
struct Shared {
    events: EventSink,
    speaker: Arc<dyn Speaker>,
    /// Without TTS the agent never speaks.
    speaks: bool,
    /// Bumped by a barge-in or a hush: sentences and audio from before are dropped.
    generation: AtomicU64,
    speech: Mutex<Speech>,
    /// Wakes the speaker worker.
    wake: Notify,
    ended: AtomicBool,
}

#[derive(Default)]
struct Speech {
    /// Sentences to speak, with the generation they were queued in.
    queue: VecDeque<(u64, String)>,
    /// The generation of the sentence being synthesized or queued on the speaker.
    busy: Option<u64>,
    /// `Speaking(true)` was reported, `Speaking(false)` not yet.
    speaking: bool,
    /// When the audio queued so far will have played.
    playing_until: Option<Instant>,
}

impl Shared {
    fn emit(&self, event: SessionEvent) {
        (self.events)(event);
    }

    fn speech(&self) -> MutexGuard<'_, Speech> {
        self.speech
            .lock()
            .expect("the speech state is not poisoned")
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn speaking(&self) -> bool {
        let speech = self.speech();
        speech.speaking || speech.busy == Some(self.generation()) || !speech.queue.is_empty()
    }

    /// Stops speaking at once; whether `Speaking(true)` was reported (and so
    /// `Speaking(false)` is due).
    fn stop_speaking(&self) -> bool {
        let was_speaking = {
            let mut speech = self.speech();
            self.generation.fetch_add(1, Ordering::AcqRel);
            speech.queue.clear();
            speech.playing_until = None;
            std::mem::take(&mut speech.speaking)
        };
        self.speaker.clear();
        self.wake.notify_one();
        was_speaking
    }
}

impl Session {
    /// Runs the call's pipeline on the current runtime until it ends.
    pub fn start(
        line: Line,
        stt: Arc<dyn Stt>,
        tts: Option<Arc<dyn Tts>>,
        vad: VadConfig,
        events: EventSink,
    ) -> Self {
        let Line {
            call_id,
            incoming,
            speaker,
            control,
        } = line;
        let shared = Arc::new(Shared {
            events,
            speaker,
            speaks: tts.is_some(),
            generation: AtomicU64::new(0),
            speech: Mutex::new(Speech::default()),
            wake: Notify::new(),
            ended: AtomicBool::new(false),
        });
        let (utterances, to_transcribe) = mpsc::unbounded_channel();
        let mut workers: Vec<JoinHandle<()>> = vec![
            tokio::spawn(listen(shared.clone(), incoming, Vad::new(vad), utterances)),
            tokio::spawn(transcribe(shared.clone(), stt, to_transcribe)),
        ];
        if let Some(tts) = tts {
            workers.push(tokio::spawn(speak(shared.clone(), tts)));
        }
        let mut tasks: Vec<AbortHandle> = workers.iter().map(JoinHandle::abort_handle).collect();
        let watcher = tokio::spawn(watch(shared.clone(), control.clone(), workers));
        tasks.push(watcher.abort_handle());
        Self {
            call_id,
            shared,
            control,
            tasks,
        }
    }

    pub fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Queues one sentence to speak (ignored without TTS).
    pub fn say(&self, text: impl Into<String>) {
        if !self.shared.speaks || self.shared.ended.load(Ordering::Acquire) {
            return;
        }
        let generation = self.shared.generation();
        self.shared
            .speech()
            .queue
            .push_back((generation, text.into()));
        self.shared.wake.notify_one();
    }

    /// Stops speaking: drops the queued sentences and the queued audio.
    pub fn hush(&self) {
        if self.shared.stop_speaking() {
            self.shared.emit(SessionEvent::Speaking(false));
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
        self.shared.speaking()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        if !self.shared.ended.load(Ordering::Acquire) {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let control = self.control.clone();
                runtime.spawn(async move {
                    control.hangup().await;
                });
            }
        }
    }
}

/// Reports the connection and the end, then stops the workers.
async fn watch(shared: Arc<Shared>, control: Arc<dyn LineControl>, workers: Vec<JoinHandle<()>>) {
    let end = match control.connected().await {
        Ok(()) => {
            shared.emit(SessionEvent::Connected);
            control.ended().await
        }
        Err(end) => end,
    };
    shared.ended.store(true, Ordering::Release);
    for worker in &workers {
        worker.abort();
    }
    for worker in workers {
        let _ = worker.await;
    }
    tracing::info!(
        event = "voice_call_ended",
        reason = end.reason.as_str(),
        duration_s = end.duration_s
    );
    shared.emit(SessionEvent::Ended(end));
}

/// Runs the detector on the owner's audio.
async fn listen(
    shared: Arc<Shared>,
    mut incoming: mpsc::Receiver<Frame>,
    mut vad: Vad,
    utterances: mpsc::UnboundedSender<Vec<i16>>,
) {
    while let Some(frame) = incoming.recv().await {
        match vad.push(&frame) {
            Some(VadEvent::SpeechStarted) => {
                if shared.speaking() {
                    let was_speaking = shared.stop_speaking();
                    tracing::info!(event = "voice_barge_in");
                    shared.emit(SessionEvent::BargedIn);
                    if was_speaking {
                        shared.emit(SessionEvent::Speaking(false));
                    }
                }
                shared.emit(SessionEvent::Listening(true));
            }
            Some(VadEvent::Utterance(samples)) => {
                shared.emit(SessionEvent::Listening(false));
                if utterances.send(samples).is_err() {
                    return;
                }
            }
            Some(VadEvent::Discarded) => shared.emit(SessionEvent::Listening(false)),
            None => {}
        }
    }
}

/// Transcribes the utterances one at a time, in order.
async fn transcribe(
    shared: Arc<Shared>,
    stt: Arc<dyn Stt>,
    mut utterances: mpsc::UnboundedReceiver<Vec<i16>>,
) {
    while let Some(samples) = utterances.recv().await {
        let ms = samples.len() as u64 * 1000 / u64::from(SAMPLE_RATE);
        tracing::info!(event = "voice_utterance", ms);
        match stt.transcribe(&samples).await {
            Ok(text) => {
                let text = text.trim();
                if !text.is_empty() {
                    shared.emit(SessionEvent::Transcript(text.to_owned()));
                }
            }
            Err(error) => {
                tracing::warn!(
                    event = "stt_failed",
                    provider = error.provider,
                    code = %error.code
                );
                shared.emit(SessionEvent::SpeechFailed(error.code));
            }
        }
    }
}

/// What the speaker worker does next.
enum Next {
    Sentence(u64, String),
    /// Nothing queued: the audio plays until then.
    Playing(Instant),
    /// The audio played: `Speaking(false)` is due.
    Done,
    Idle,
}

/// Speaks the queued sentences.
async fn speak(shared: Arc<Shared>, tts: Arc<dyn Tts>) {
    loop {
        let next = {
            let mut speech = shared.speech();
            if let Some((generation, text)) = speech.queue.pop_front() {
                speech.busy = Some(generation);
                Next::Sentence(generation, text)
            } else if !speech.speaking {
                Next::Idle
            } else {
                match speech.playing_until {
                    Some(until) if until > Instant::now() => Next::Playing(until),
                    _ => {
                        speech.speaking = false;
                        speech.playing_until = None;
                        Next::Done
                    }
                }
            }
        };
        match next {
            Next::Sentence(generation, text) => {
                let spoken = say_sentence(&shared, tts.as_ref(), generation, &text).await;
                shared.speech().busy = None;
                if !spoken {
                    return;
                }
            }
            Next::Playing(until) => {
                tokio::select! {
                    () = tokio::time::sleep_until(until) => {}
                    () = shared.wake.notified() => {}
                }
            }
            Next::Done => shared.emit(SessionEvent::Speaking(false)),
            Next::Idle => shared.wake.notified().await,
        }
    }
}

/// Synthesizes `text` and queues its audio unless a barge-in or a hush overtakes it;
/// `false` once the call ended.
async fn say_sentence(shared: &Shared, tts: &dyn Tts, generation: u64, text: &str) -> bool {
    if generation != shared.generation() {
        return true;
    }
    let samples = match tts.synthesize(text).await {
        Ok(samples) => samples,
        Err(error) => {
            tracing::warn!(
                event = "tts_failed",
                provider = error.provider,
                code = %error.code
            );
            return true;
        }
    };
    let started = {
        let mut speech = shared.speech();
        if generation != shared.generation() {
            return true;
        }
        !std::mem::replace(&mut speech.speaking, true)
    };
    if started {
        shared.emit(SessionEvent::Speaking(true));
    }
    let frames = wav::frames(&samples);
    tracing::info!(
        event = "voice_sentence",
        ms = frames.len() as u64 * FRAME_TIME.as_millis() as u64
    );
    for frame in frames {
        if generation != shared.generation() {
            return true;
        }
        if !shared.speaker.send(frame).await {
            return false;
        }
        let mut speech = shared.speech();
        if generation != shared.generation() {
            return true;
        }
        let now = Instant::now();
        let from = speech.playing_until.map_or(now, |until| until.max(now));
        speech.playing_until = Some(from + FRAME_TIME);
    }
    true
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

    fn utterance(hz: f64) -> Vec<i16> {
        let mut audio = silence(200);
        audio.extend(tone(hz, 900, 8000.0));
        audio.extend(silence(900));
        audio
    }

    #[tokio::test]
    async fn an_utterance_becomes_a_transcript() {
        let mut call = call().await;
        call.owner.say(&utterance(900.0)).await;
        let seen = call
            .until(&SessionEvent::Transcript("approve".into()))
            .await;
        assert_eq!(
            seen,
            vec![
                SessionEvent::Listening(true),
                SessionEvent::Listening(false),
                SessionEvent::Transcript("approve".into()),
            ]
        );
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
        let seen = call.until(&SessionEvent::Transcript("hello".into())).await;
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
        call.until(&SessionEvent::Transcript("hello".into())).await;
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
                .any(|e| matches!(e, SessionEvent::Transcript(_)))
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
