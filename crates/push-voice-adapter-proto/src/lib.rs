//! The voice adapter protocol, `push-voice-adapter/1`: how push-agent-bridge carries a
//! call's audio to and from a separate voice adapter process (speech recognition and
//! synthesis), and asks it to speak (docs/voice-adapter-protocol.md).
//!
//! - [`frame`]: length-prefixed frames (`u32` big-endian payload length, `u8` kind,
//!   payload): JSON control (kind 1) and audio (kind 2), each at most 64 KiB; a sans-IO
//!   incremental [`FrameDecoder`].
//! - [`io`] and, with the default feature `tokio`, [`tokio_io`]: blocking and async frame
//!   readers and writers.
//! - [`message`]: the typed control messages of both directions, their validation, and
//!   decoding that ignores unknown fields and message types.
//! - [`version`], [`format`] and [`limits`]: the protocol version and its negotiation, the
//!   fixed v1 audio format, and every cap and timer.
//! - [`schema`]: the JSON Schemas of the control messages (checked in under
//!   `docs/schemas/voice-adapter/v1/`).
//!
//! Transcripts and text to speak are content: no `Debug` impl or error of this crate
//! prints them.

pub mod format;
pub mod frame;
pub mod io;
pub mod limits;
pub mod message;
pub mod schema;
#[cfg(feature = "tokio")]
pub mod tokio_io;
pub mod version;

pub use format::AudioFormat;
pub use frame::{AudioFrame, Frame, FrameDecoder, FrameError, Kind, Stream};
pub use io::{FrameReader, FrameWriter, ReadError, WriteError};
pub use message::{
    AdapterHello, AdapterMessage, AudioSpec, BridgeHello, BridgeMessage, CallEndReason, CallEnded,
    CallStarted, Capabilities, Decoded, ErrorCode, ErrorMessage, Invalid, Message, MessageError,
    Ping, Pong, Speak, SpeakDone, SpeechStarted, StopSpeaking, Transcript, Voice, decode, encode,
};
#[cfg(feature = "tokio")]
pub use tokio_io::{AsyncFrameReader, AsyncFrameWriter};
pub use version::{PROTOCOL, ProtocolVersion, SUPPORTED, negotiate};
