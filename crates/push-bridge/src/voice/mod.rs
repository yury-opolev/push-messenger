//! Voice calls with the owner. Speech runs in a voice adapter, a separate program speaking
//! push-voice-adapter/1 ([`adapter`]: started or connected to, supervised, and routing a
//! call's speech through it); the bridge itself has no speech engine. What stays here is
//! the conversation: the owner's transcripts become prompts, the agent's Markdown reply
//! becomes short plain sentences to speak ([`speech`]), open approval and question cards
//! can be answered by voice ([`confirm`]), and when calls are answered and placed
//! ([`policy`]). [`phone`] answers calls (push-sdk's media helper in `media`, feature
//! `voice`; a fake in tests) and [`session`] runs one call's pipeline: the owner's audio
//! to the adapter, its transcripts back, the sentences to speak and barge-in, with the
//! speech in an [`engine`].

pub mod adapter;
pub mod confirm;
pub mod engine;
#[cfg(feature = "voice")]
pub mod media;
pub mod phone;
pub mod policy;
pub mod session;
pub mod speech;

use std::sync::Arc;

/// What the bridge needs to take calls: the phone, how calls are answered and placed, and
/// the voice adapter that runs the speech. Calls are taken while the adapter is ready
/// with `stt`; it speaks when it has `tts`.
#[derive(Clone)]
pub struct VoiceSetup {
    pub phone: Arc<dyn phone::Phone>,
    pub policy: policy::CallPolicy,
    pub adapter: adapter::router::AdapterSetup,
}

impl VoiceSetup {
    /// Whether a call could be listened to now: the voice adapter is ready with `stt`.
    pub fn can_listen(&self) -> bool {
        self.adapter.can_listen()
    }

    /// Whether a call could say something now: the voice adapter is ready with `tts`.
    pub fn can_speak(&self) -> bool {
        self.adapter.can_speak()
    }

    /// Starts the pipeline of `line`, its speech in the voice adapter. `peer`: the
    /// owner's display name (for the adapter).
    pub fn start_session(
        &self,
        line: phone::Line,
        peer: &str,
        events: session::EventSink,
    ) -> session::Session {
        session::Session::with_adapter(line, &self.adapter, peer, events)
    }
}

/// Samples in one 20 ms frame of call audio (16 kHz mono, push-sdk's media helper).
pub const FRAME: usize = 320;
/// One 20 ms frame of 16 kHz mono audio.
pub type Frame = [i16; FRAME];

/// `samples` as call frames, the last one padded with silence.
pub fn frames(samples: &[i16]) -> Vec<Frame> {
    samples
        .chunks(FRAME)
        .map(|chunk| {
            let mut frame = [0; FRAME];
            frame[..chunk.len()].copy_from_slice(chunk);
            frame
        })
        .collect()
}
