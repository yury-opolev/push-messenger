//! Voice calls with the owner: call audio (16 kHz mono, 20 ms frames) is cut into
//! utterances by voice activity detection, transcribed and sent to the agent as a prompt;
//! the agent's Markdown reply becomes short plain sentences that are synthesized and played
//! back; open approval and question cards can be answered by voice. These are the pure
//! building blocks: WAV files and resampling, the detector, the spoken text and voice
//! answers. [`provider`] runs the speech services: Azure AI Speech, local servers or
//! commands, and tone-based fakes for tests. [`phone`] answers calls (push-sdk's media
//! helper in `media`, feature `voice`; a fake in tests) and [`session`] runs one call's
//! pipeline: listening, transcribing, speaking, barge-in.

pub mod azure;
pub mod confirm;
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
/// is declined; without `tts` the agent's replies stay in the chat.
#[derive(Clone)]
pub struct VoiceSetup {
    pub phone: Arc<dyn phone::Phone>,
    pub stt: Option<Arc<dyn provider::Stt>>,
    pub tts: Option<Arc<dyn provider::Tts>>,
    pub vad: vad::VadConfig,
    pub policy: policy::CallPolicy,
}

/// Samples per second of call audio (push-sdk's media helper).
pub const SAMPLE_RATE: u32 = 16_000;
/// Samples in one 20 ms frame.
pub const FRAME: usize = 320;
/// One 20 ms frame of 16 kHz mono audio.
pub type Frame = [i16; FRAME];
