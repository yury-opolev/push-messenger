//! A call's speech, behind [`SpeechEngine`]: the [`Session`](super::session::Session)
//! keeps the line (the owner's audio in, the [`Speaker`], the hang-up), the sentences
//! to speak, the barge-in generation, mute and silence, and the [`SessionEvent`]s; an
//! engine hears the owner's audio, turns it into transcripts and speaks the queued
//! sentences.
//!
//! The engine is the [`AdapterEngine`](super::adapter::router::AdapterEngine): speech in
//! the voice adapter (push-voice-adapter/1); tests may plug in others.
//!
//! The session and its engine share a [`SpeechContext`]: the session queues sentences in
//! it (with the generation they belong to) and wakes [`SpeechContext::wake`]; the engine
//! takes them from there. A barge-in or a hush bumps the generation, drops the queue and clears the
//! speaker ([`SpeechContext::stop_speaking`]), then the session calls
//! [`SpeechEngine::stop`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use push_sdk::CallEndReason;
use tokio::sync::Notify;
use tokio::time::Instant;

use super::Frame;
use super::phone::Speaker;
use super::session::{EventSink, SessionEvent};

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
