//! A call's speech, behind [`SpeechEngine`]: the [`Session`](super::session::Session)
//! keeps the line (the owner's audio in, the [`Speaker`], the hang-up), the sentences
//! to speak, the barge-in generation, mute and silence, and the [`SessionEvent`]s; an
//! engine hears the owner's audio, turns it into transcripts and speaks the queued
//! sentences.
//!
//! - [`BuiltinEngine`]: the bridge's own speech (the detector, the STT and TTS
//!   providers), removed with them in PR 3.
//! - [`AdapterEngine`](super::adapter::router::AdapterEngine): speech in the voice
//!   adapter (push-voice-adapter/1).
//!
//! Both share a [`SpeechContext`]: the session queues sentences in it (with the
//! generation they belong to) and wakes [`SpeechContext::wake`]; the engine takes them
//! from there. A barge-in or a hush bumps the generation, drops the queue and clears the
//! speaker ([`SpeechContext::stop_speaking`]), then the session calls
//! [`SpeechEngine::stop`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use push_sdk::CallEndReason;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::phone::Speaker;
use super::provider::{Stt, Tts};
use super::session::{EventSink, SessionEvent};
use super::vad::{Vad, VadConfig, VadEvent};
use super::{Frame, SAMPLE_RATE, wav};

/// One frame of call audio.
pub(crate) const FRAME_TIME: Duration = Duration::from_millis(20);

/// Speech for one call: hears the line's audio, reports [`SessionEvent`]s through the
/// session's [`SpeechContext`], speaks the sentences queued there.
#[async_trait::async_trait]
pub trait SpeechEngine: Send + Sync + 'static {
    /// The owner's frame (from the line's incoming audio).
    fn hear(&self, frame: &Frame);
    /// Whether sentences can be spoken now; the session drops them otherwise (the replies
    /// stay in the chat).
    fn speaks(&self) -> bool;
    /// Audio flows: the call is up.
    fn connected(&self);
    /// Speech was cut (a barge-in, a hush, a mute): the context already dropped the queue
    /// and cleared the speaker; drop what the engine still has.
    fn stop(&self);
    /// The call is over: stop and return once the engine's tasks have stopped (no event
    /// is reported after).
    async fn end(&self, reason: CallEndReason);
    /// The bridge stops during the call: an engine with a peer of its own tells it so
    /// (the voice adapter's `call_ended {reason: shutdown}`) and returns once it did.
    /// Nothing by default.
    async fn shut_down(&self) {}
}

/// A sentence to speak.
pub(crate) struct Sentence {
    /// The generation it was queued in.
    pub(crate) generation: u64,
    pub(crate) text: String,
    /// The bridge's own line (a greeting, a card, a confirmation), not the agent's reply.
    pub(crate) own: bool,
}

/// What the session and its engine share.
pub(crate) struct SpeechContext {
    events: EventSink,
    pub(crate) speaker: Arc<dyn Speaker>,
    /// Bumped by a barge-in or a hush: sentences and audio from before are dropped.
    generation: AtomicU64,
    speech: Mutex<Speech>,
    /// Wakes the engine's speaker: a sentence was queued, or speech was cut.
    pub(crate) wake: Notify,
    ended: AtomicBool,
    /// The owner muted the agent (the tray's Mute agent): nothing is spoken.
    pub(crate) silenced: AtomicBool,
}

#[derive(Default)]
pub(crate) struct Speech {
    /// Sentences to speak.
    pub(crate) queue: VecDeque<Sentence>,
    /// The generation of the sentence being synthesized or queued on the speaker (the
    /// adapter: of the speaks it has not finished).
    pub(crate) busy: Option<u64>,
    /// `Speaking(true)` was reported, `Speaking(false)` not yet.
    pub(crate) speaking: bool,
    /// When the audio queued so far will have played.
    pub(crate) playing_until: Option<Instant>,
    /// A barge-in cut the reply: [`Session::say_reply`](super::session::Session::say_reply)
    /// drops sentences until [`Session::unmute`](super::session::Session::unmute).
    pub(crate) muted: bool,
}

impl Speech {
    /// Whether sentences of `generation` are queued, synthesized or playing.
    pub(crate) fn active(&self, generation: u64) -> bool {
        self.speaking || self.busy == Some(generation) || !self.queue.is_empty()
    }
}

impl SpeechContext {
    pub(crate) fn new(events: EventSink, speaker: Arc<dyn Speaker>) -> Self {
        Self {
            events,
            speaker,
            generation: AtomicU64::new(0),
            speech: Mutex::new(Speech::default()),
            wake: Notify::new(),
            ended: AtomicBool::new(false),
            silenced: AtomicBool::new(false),
        }
    }

    pub(crate) fn emit(&self, event: SessionEvent) {
        (self.events)(event);
    }

    pub(crate) fn speech(&self) -> MutexGuard<'_, Speech> {
        self.speech
            .lock()
            .expect("the speech state is not poisoned")
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    pub(crate) fn set_ended(&self) {
        self.ended.store(true, Ordering::Release);
    }

    pub(crate) fn speaking(&self) -> bool {
        self.speech().active(self.generation())
    }

    /// Stops speaking at once, reporting `Speaking(false)` if `Speaking(true)` was
    /// reported. A barge-in only stops an agent that speaks, mutes the reply and reports
    /// `BargedIn` first; whether it did. Nothing happens once the call ended.
    ///
    /// The events are emitted under the speech lock, as the engines' are: a
    /// `Speaking(true)` can never follow the `Speaking(false)` that ends it.
    pub(crate) fn stop_speaking(&self, barge_in: bool) -> bool {
        {
            let mut speech = self.speech();
            if self.ended() || (barge_in && !speech.active(self.generation())) {
                return false;
            }
            self.generation.fetch_add(1, Ordering::AcqRel);
            speech.queue.clear();
            speech.playing_until = None;
            if barge_in {
                speech.muted = true;
                self.emit(SessionEvent::BargedIn);
            }
            if std::mem::take(&mut speech.speaking) {
                self.emit(SessionEvent::Speaking(false));
            }
        }
        self.speaker.clear();
        self.wake.notify_one();
        true
    }
}

/// The bridge's own speech: the detector cuts the owner's audio into utterances, the STT
/// worker transcribes them one at a time, the speaker worker synthesizes the queued
/// sentences (without TTS nothing is spoken).
pub struct BuiltinEngine {
    ctx: Arc<SpeechContext>,
    listener: Mutex<Listener>,
    utterances: mpsc::UnboundedSender<(Vec<i16>, Instant)>,
    speaks: bool,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

struct Listener {
    vad: Vad,
    /// When the detector heard the current utterance's speech start.
    started: Instant,
}

impl BuiltinEngine {
    /// Starts the workers on the current runtime.
    pub(crate) fn start(
        ctx: Arc<SpeechContext>,
        stt: Arc<dyn Stt>,
        tts: Option<Arc<dyn Tts>>,
        vad: VadConfig,
    ) -> Self {
        let (utterances, to_transcribe) = mpsc::unbounded_channel();
        let mut workers = vec![tokio::spawn(transcribe(ctx.clone(), stt, to_transcribe))];
        let speaks = tts.is_some();
        if let Some(tts) = tts {
            workers.push(tokio::spawn(speak(ctx.clone(), tts)));
        }
        Self {
            ctx,
            listener: Mutex::new(Listener {
                vad: Vad::new(vad),
                started: Instant::now(),
            }),
            utterances,
            speaks,
            workers: Mutex::new(workers),
        }
    }

    fn take_workers(&self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut *self.workers.lock().expect("the workers are not poisoned"))
    }
}

#[async_trait::async_trait]
impl SpeechEngine for BuiltinEngine {
    /// Runs the detector: speech starting while the agent speaks is a barge-in.
    fn hear(&self, frame: &Frame) {
        let ctx = &self.ctx;
        let mut listener = self.listener.lock().expect("the listener is not poisoned");
        match listener.vad.push(frame) {
            Some(VadEvent::SpeechStarted) => {
                listener.started = Instant::now();
                if ctx.stop_speaking(true) {
                    tracing::info!(event = "voice_barge_in");
                }
                ctx.emit(SessionEvent::Listening(true));
            }
            Some(VadEvent::Utterance(samples)) => {
                ctx.emit(SessionEvent::Listening(false));
                let _ = self.utterances.send((samples, listener.started));
            }
            Some(VadEvent::Discarded) => ctx.emit(SessionEvent::Listening(false)),
            None => {}
        }
    }

    fn speaks(&self) -> bool {
        self.speaks
    }

    fn connected(&self) {}

    /// The context dropped the queue and woke the speaker worker: nothing more to do.
    fn stop(&self) {}

    async fn end(&self, _reason: CallEndReason) {
        let workers = self.take_workers();
        for worker in &workers {
            worker.abort();
        }
        for worker in workers {
            let _ = worker.await;
        }
    }
}

impl Drop for BuiltinEngine {
    fn drop(&mut self) {
        for worker in self.take_workers() {
            worker.abort();
        }
    }
}

/// Transcribes the utterances one at a time, in order.
async fn transcribe(
    ctx: Arc<SpeechContext>,
    stt: Arc<dyn Stt>,
    mut utterances: mpsc::UnboundedReceiver<(Vec<i16>, Instant)>,
) {
    while let Some((samples, started)) = utterances.recv().await {
        let ms = samples.len() as u64 * 1000 / u64::from(SAMPLE_RATE);
        tracing::info!(event = "voice_utterance", ms);
        match stt.transcribe(&samples).await {
            Ok(text) => {
                let text = text.trim();
                if !text.is_empty() {
                    ctx.emit(SessionEvent::Transcript {
                        text: text.to_owned(),
                        started,
                    });
                }
            }
            Err(error) => {
                tracing::warn!(
                    event = "stt_failed",
                    provider = error.provider,
                    code = %error.code
                );
                ctx.emit(SessionEvent::SpeechFailed(error.code));
            }
        }
    }
}

/// What the speaker worker does next.
enum Next {
    Sentence(u64, String),
    /// Nothing queued: the audio plays until then.
    Playing(Instant),
    /// The audio played: `Speaking(false)` was reported.
    Done,
    Idle,
}

/// Speaks the queued sentences.
async fn speak(ctx: Arc<SpeechContext>, tts: Arc<dyn Tts>) {
    loop {
        let next = {
            let mut speech = ctx.speech();
            if let Some(sentence) = speech.queue.pop_front() {
                speech.busy = Some(sentence.generation);
                Next::Sentence(sentence.generation, sentence.text)
            } else if !speech.speaking {
                Next::Idle
            } else {
                match speech.playing_until {
                    Some(until) if until > Instant::now() => Next::Playing(until),
                    _ => {
                        speech.speaking = false;
                        speech.playing_until = None;
                        ctx.emit(SessionEvent::Speaking(false));
                        Next::Done
                    }
                }
            }
        };
        match next {
            Next::Sentence(generation, text) => {
                let spoken = say_sentence(&ctx, tts.as_ref(), generation, &text).await;
                ctx.speech().busy = None;
                if !spoken {
                    return;
                }
            }
            Next::Playing(until) => {
                tokio::select! {
                    () = tokio::time::sleep_until(until) => {}
                    () = ctx.wake.notified() => {}
                }
            }
            Next::Done => {}
            Next::Idle => ctx.wake.notified().await,
        }
    }
}

/// Synthesizes `text` and queues its audio unless a barge-in or a hush overtakes it;
/// `false` once the call ended.
async fn say_sentence(ctx: &SpeechContext, tts: &dyn Tts, generation: u64, text: &str) -> bool {
    if generation != ctx.generation() {
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
    {
        let mut speech = ctx.speech();
        if generation != ctx.generation() {
            return true;
        }
        if !std::mem::replace(&mut speech.speaking, true) {
            ctx.emit(SessionEvent::Speaking(true));
        }
    }
    let frames = wav::frames(&samples);
    tracing::info!(
        event = "voice_sentence",
        ms = frames.len() as u64 * FRAME_TIME.as_millis() as u64
    );
    for frame in frames {
        if generation != ctx.generation() {
            return true;
        }
        if !ctx.speaker.send(frame).await {
            return false;
        }
        let mut speech = ctx.speech();
        if generation != ctx.generation() {
            drop(speech);
            // A barge-in or a hush overtook the frame while the speaker took it: its
            // `clear()` may have run before the frame was queued, which then carries the
            // speaker's new generation and would play after the cut. Clear again.
            ctx.speaker.clear();
            return true;
        }
        let now = Instant::now();
        let from = speech.playing_until.map_or(now, |until| until.max(now));
        speech.playing_until = Some(from + FRAME_TIME);
    }
    true
}
