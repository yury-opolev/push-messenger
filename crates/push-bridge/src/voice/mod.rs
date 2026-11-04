//! Voice calls with the owner: call audio (16 kHz mono, 20 ms frames) is cut into
//! utterances by voice activity detection, transcribed and sent to the agent as a prompt;
//! the agent's Markdown reply becomes short plain sentences that are synthesized and played
//! back; open approval and question cards can be answered by voice. These are the pure
//! building blocks: WAV files and resampling, the detector, the spoken text and voice
//! answers. [`provider`] runs the speech services: Azure AI Speech, local servers or
//! commands, and tone-based fakes for tests. [`phone`] answers calls (push-sdk's media
//! helper in `media`, feature `voice`; a fake in tests) and [`session`] runs one call's
//! pipeline: listening, transcribing, speaking, barge-in, with its speech in an [`engine`].
//! [`adapter`] starts or connects to a voice adapter, a separate program that runs speech
//! (push-voice-adapter/1), and routes a call's speech through it.

pub mod adapter;
pub mod azure;
pub mod confirm;
pub mod engine;
pub mod fake;
pub mod local;
#[cfg(feature = "voice")]
pub mod media;
pub mod phone;
pub mod policy;
pub mod provider;
pub mod session;
pub mod speech;
#[cfg(test)]
pub(crate) mod test_server;
pub mod vad;
pub mod wav;

use std::sync::Arc;

/// What the bridge needs to take calls: the phone, the speech providers and the
/// detector's settings, and how calls are answered and placed. Without `stt` every call
/// is declined; without `tts` the agent's replies stay in the chat. With `adapter` speech
/// runs in the voice adapter instead (`stt` and `tts` are then `None`): calls can be taken
/// while it is ready with `stt`, and it speaks when it has `tts`.
#[derive(Clone)]
pub struct VoiceSetup {
    pub phone: Arc<dyn phone::Phone>,
    pub stt: Option<Arc<dyn provider::Stt>>,
    pub tts: Option<Arc<dyn provider::Tts>>,
    pub vad: vad::VadConfig,
    pub policy: policy::CallPolicy,
    pub adapter: Option<adapter::router::AdapterSetup>,
}

impl VoiceSetup {
    /// Whether a call could be listened to now: a voice adapter ready with `stt`, or the
    /// built-in STT.
    pub fn can_listen(&self) -> bool {
        match &self.adapter {
            Some(adapter) => adapter.can_listen(),
            None => self.stt.is_some(),
        }
    }

    /// Starts the pipeline of `line`: in the voice adapter when there is one, else with
    /// the built-in providers; `None` without either STT. `peer`: the owner's display
    /// name (for the adapter).
    pub fn start_session(
        &self,
        line: phone::Line,
        peer: &str,
        events: session::EventSink,
    ) -> Option<session::Session> {
        if let Some(adapter) = &self.adapter {
            return Some(session::Session::with_adapter(line, adapter, peer, events));
        }
        let stt = self.stt.clone()?;
        Some(session::Session::start(
            line,
            stt,
            self.tts.clone(),
            self.vad,
            events,
        ))
    }
}

/// Samples per second of call audio (push-sdk's media helper).
pub const SAMPLE_RATE: u32 = 16_000;
/// Samples in one 20 ms frame.
pub const FRAME: usize = 320;
/// One 20 ms frame of 16 kHz mono audio.
pub type Frame = [i16; FRAME];
