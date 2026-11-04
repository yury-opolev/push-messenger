//! The control messages (kind-1 frames): one JSON object each, with a string `type`.
//!
//! [`BridgeMessage`] is what the bridge sends, [`AdapterMessage`] what the adapter sends;
//! `error`, `ping` and `pong` go both ways. [`decode`] ignores unknown fields and returns
//! [`Decoded::Unknown`] for unknown message types, so the protocol can grow; a known type
//! that is malformed or breaks a cap is a [`MessageError`]. [`encode`] validates before
//! encoding. Unknown `error.code` and `call_ended.reason` values decode as `other`.
//!
//! Content-free: `Debug` shows the lengths of transcripts, text to speak, the peer's name
//! and error messages, never the text; [`MessageError`] never echoes its input.

use std::fmt;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::frame::Frame;
use crate::limits::{
    MAX_ERROR_MESSAGE_CHARS, MAX_ID_LEN, MAX_LANGUAGE_LEN, MAX_NAME_CHARS, MAX_PAYLOAD,
    MAX_PROTOCOLS, MAX_TEXT_CHARS, MAX_VOICES,
};
use crate::version::{PROTOCOL, SUPPORTED};

/// A direction's control messages.
pub trait Message: Serialize + DeserializeOwned + sealed::Sealed {
    /// Every `type` of this direction.
    const TYPES: &'static [&'static str];

    /// This message's `type`.
    fn message_type(&self) -> &'static str;

    /// Checks every cap and the shape of ids, names and languages.
    fn validate(&self) -> Result<(), Invalid>;
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::BridgeMessage {}
    impl Sealed for super::AdapterMessage {}
}

/// An optional list: absent and `null` are both empty.
fn null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<Vec<T>>::deserialize(deserializer).map(Option::unwrap_or_default)
}

/// What the bridge sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeMessage {
    Hello(BridgeHello),
    CallStarted(CallStarted),
    Speak(Speak),
    StopSpeaking(StopSpeaking),
    CallEnded(CallEnded),
    Error(ErrorMessage),
    Ping(Ping),
    Pong(Pong),
}

/// What the adapter sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdapterMessage {
    Hello(AdapterHello),
    SpeechStarted(SpeechStarted),
    Transcript(Transcript),
    SpeakDone(SpeakDone),
    Error(ErrorMessage),
    Ping(Ping),
    Pong(Pong),
}

/// The bridge's first message on every connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BridgeHello {
    /// The bridge's preferred protocol version, `push-voice-adapter/<n>`.
    #[schemars(length(min = 1, max = 64))]
    pub protocol: String,
    /// Every version the bridge speaks (including `protocol`). Absent: only `protocol`.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_as_empty"
    )]
    #[schemars(with = "Option<Vec<String>>", length(max = 16))]
    pub protocols: Vec<String>,
    /// The bridge's version, for the adapter's diagnostics.
    #[schemars(length(max = 64))]
    pub bridge_version: String,
    /// The language spoken on calls, a BCP 47 tag such as `en-US`.
    #[schemars(regex(pattern = r"^[A-Za-z0-9-]{1,35}$"))]
    pub language: String,
    /// The call audio's format: in v1 always 16000 Hz, 1 channel, s16le, 20 ms.
    pub audio: AudioSpec,
}

/// An audio format as the bridge's hello describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AudioSpec {
    /// Samples per second: 16000.
    pub rate: u32,
    /// Channels: 1.
    pub channels: u32,
    /// Sample format: `s16le` (signed 16-bit little-endian).
    #[schemars(length(min = 1, max = 16))]
    pub format: String,
    /// Milliseconds of audio per frame: 20.
    pub frame_ms: u32,
}

/// The adapter's answer to the bridge's hello.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AdapterHello {
    /// The protocol version chosen, one the bridge offered.
    #[schemars(length(min = 1, max = 64))]
    pub protocol: String,
    /// The adapter's name, for the bridge's status.
    #[schemars(length(min = 1, max = 64))]
    pub name: String,
    /// The adapter's version, for the bridge's status.
    #[schemars(length(max = 64))]
    pub version: String,
    /// What the adapter can do. The bridge advertises calls only with `stt`.
    pub capabilities: Capabilities,
    /// Voices `speak.voice` may name.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "null_as_empty"
    )]
    #[schemars(with = "Option<Vec<Voice>>", length(max = 256))]
    pub voices: Vec<Voice>,
}

/// An adapter's capabilities; absent ones are false, unknown ones ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Capabilities {
    /// It transcribes the caller (sends `transcript`).
    #[serde(default)]
    pub stt: bool,
    /// It speaks (`speak` answers stream-1 audio).
    #[serde(default)]
    pub tts: bool,
    /// It detects speech (sends `speech_started`).
    #[serde(default)]
    pub vad: bool,
    /// It stops its own speech when the caller speaks.
    #[serde(default)]
    pub barge_in: bool,
    /// It sends partial transcripts (`final: false`).
    #[serde(default)]
    pub partials: bool,
}

/// A voice the adapter offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Voice {
    #[schemars(length(min = 1, max = 64))]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^[A-Za-z0-9-]{1,35}$"))]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 64))]
    pub name: Option<String>,
}

/// A call starts (or continues with a reconnected adapter): listen to stream 0.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CallStarted {
    /// Opaque, chosen by the bridge per call: 1–64 printable ASCII characters.
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub call_id: String,
    /// The caller's display name only.
    #[schemars(length(max = 64))]
    pub peer: String,
    /// The language spoken, BCP 47.
    #[schemars(regex(pattern = r"^[A-Za-z0-9-]{1,35}$"))]
    pub language: String,
}

/// Speak this text after any earlier unfinished speak.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Speak {
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub call_id: String,
    /// Opaque, chosen by the bridge: 1–64 printable ASCII characters.
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub speak_id: String,
    /// Plain text prepared for speech, 1–4000 characters.
    #[schemars(length(min = 1, max = 4000))]
    pub text: String,
    /// Whether the adapter may cut it on its own barge-in; `stop_speaking` always does.
    pub interruptible: bool,
    /// A voice from the adapter's hello; absent: its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 64))]
    pub voice: Option<String>,
}

/// Stop one speak, or every unfinished one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StopSpeaking {
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub call_id: String,
    /// Absent: every unfinished speak.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub speak_id: Option<String>,
}

/// The call is over: release its state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CallEnded {
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub call_id: String,
    pub reason: CallEndReason,
}

/// Why a call ended. Unknown values decode as `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallEndReason {
    /// Either side hung up.
    Completed,
    /// The media or signalling failed.
    Failed,
    /// The bridge is stopping.
    Shutdown,
    /// Never sent: what a reason this version does not know decodes as.
    #[serde(other)]
    #[schemars(skip)]
    Other,
}

/// The caller started speaking (adapters with `vad`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SpeechStarted {
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub call_id: String,
    /// Opaque, chosen by the adapter: 1–64 printable ASCII characters.
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub utterance_id: String,
}

/// What the caller said: partial (`final: false`) or final.
#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Transcript {
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub call_id: String,
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub utterance_id: String,
    /// The words, 1–4000 characters.
    #[schemars(length(min = 1, max = 4000))]
    pub text: String,
    /// Final: the bridge acts on it. One per utterance.
    #[serde(rename = "final")]
    pub is_final: bool,
    /// 0 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0.0, max = 1.0))]
    pub confidence: Option<f32>,
    /// The language recognised, BCP 47.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^[A-Za-z0-9-]{1,35}$"))]
    pub language: Option<String>,
}

/// A speak finished (after its last stream-1 frame) or was cut or dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SpeakDone {
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub call_id: String,
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub speak_id: String,
    /// Stopped, dropped or failed before its end.
    pub interrupted: bool,
}

/// Something failed. The message is content-free: no audio, transcript or speech text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorMessage {
    pub code: ErrorCode,
    /// A short, content-free explanation (at most 512 characters, no control characters).
    #[serde(default)]
    #[schemars(length(max = 512))]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub speak_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^[!-~]{1,64}$"))]
    pub utterance_id: Option<String>,
}

/// Error codes. Unknown values decode as `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No common protocol version (the sender closes).
    UnsupportedProtocol,
    /// The audio format cannot be carried (in place of a hello: the sender closes).
    UnsupportedFormat,
    /// A framing error (the sender closes).
    InvalidFrame,
    /// A control message was malformed and ignored.
    InvalidMessage,
    /// The adapter's engine cannot work right now.
    EngineUnavailable,
    /// Too many messages or too much audio.
    Overloaded,
    /// An utterance could not be transcribed.
    RecognitionFailed,
    /// A speak could not be synthesized (with `speak_id`).
    SynthesisFailed,
    /// A `call_id` that is not active.
    UnknownCall,
    /// A second call, or a second bridge on one adapter.
    Busy,
    /// Anything else.
    Internal,
    /// Never sent: what a code this version does not know decodes as.
    #[serde(other)]
    #[schemars(skip)]
    Other,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::UnsupportedProtocol => "unsupported_protocol",
            ErrorCode::UnsupportedFormat => "unsupported_format",
            ErrorCode::InvalidFrame => "invalid_frame",
            ErrorCode::InvalidMessage => "invalid_message",
            ErrorCode::EngineUnavailable => "engine_unavailable",
            ErrorCode::Overloaded => "overloaded",
            ErrorCode::RecognitionFailed => "recognition_failed",
            ErrorCode::SynthesisFailed => "synthesis_failed",
            ErrorCode::UnknownCall => "unknown_call",
            ErrorCode::Busy => "busy",
            ErrorCode::Internal => "internal",
            ErrorCode::Other => "other",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Liveness: answer with a `pong` of the same id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Ping {
    pub id: u32,
}

/// The answer to a `ping`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Pong {
    pub id: u32,
}

impl BridgeHello {
    /// The hello of a bridge speaking this crate's versions, with the v1 audio format.
    pub fn v1(bridge_version: impl Into<String>, language: impl Into<String>) -> Self {
        Self {
            protocol: PROTOCOL.to_owned(),
            protocols: SUPPORTED.iter().map(|v| (*v).to_owned()).collect(),
            bridge_version: bridge_version.into(),
            language: language.into(),
            audio: crate::format::AudioFormat::V1.spec(),
        }
    }

    /// The versions offered: `protocols`, or only `protocol` when that is empty.
    pub fn offered(&self) -> Vec<&str> {
        if self.protocols.is_empty() {
            vec![self.protocol.as_str()]
        } else {
            self.protocols.iter().map(String::as_str).collect()
        }
    }
}

impl AdapterHello {
    /// A hello speaking this crate's version.
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        capabilities: Capabilities,
    ) -> Self {
        Self {
            protocol: PROTOCOL.to_owned(),
            name: name.into(),
            version: version.into(),
            capabilities,
            voices: Vec::new(),
        }
    }
}

impl ErrorMessage {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            call_id: None,
            speak_id: None,
            utterance_id: None,
        }
    }

    pub fn for_call(mut self, call_id: impl Into<String>) -> Self {
        self.call_id = Some(call_id.into());
        self
    }
}

/// Lengths only: content never reaches a log through `Debug`.
struct Redacted<'a>(&'a str);

impl fmt::Debug for Redacted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} chars>", self.0.chars().count())
    }
}

impl fmt::Debug for CallStarted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallStarted")
            .field("call_id", &self.call_id)
            .field("peer", &Redacted(&self.peer))
            .field("language", &self.language)
            .finish()
    }
}

impl fmt::Debug for Speak {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Speak")
            .field("call_id", &self.call_id)
            .field("speak_id", &self.speak_id)
            .field("text", &Redacted(&self.text))
            .field("interruptible", &self.interruptible)
            .field("voice", &self.voice)
            .finish()
    }
}

impl fmt::Debug for Transcript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transcript")
            .field("call_id", &self.call_id)
            .field("utterance_id", &self.utterance_id)
            .field("text", &Redacted(&self.text))
            .field("final", &self.is_final)
            .field("confidence", &self.confidence)
            .field("language", &self.language)
            .finish()
    }
}

impl fmt::Debug for ErrorMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ErrorMessage")
            .field("code", &self.code)
            .field("message", &Redacted(&self.message))
            .field("call_id", &self.call_id)
            .field("speak_id", &self.speak_id)
            .field("utterance_id", &self.utterance_id)
            .finish()
    }
}

/// A message's field breaks a rule. Content-free: names the field and the rule only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("`{field}` {problem}")]
pub struct Invalid {
    pub field: &'static str,
    pub problem: Problem,
}

/// Which rule a field breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("is empty")]
    Empty,
    #[error("is too long")]
    TooLong,
    #[error("has characters it may not have")]
    BadCharacters,
    #[error("is out of range")]
    OutOfRange,
    #[error("has too many items")]
    TooMany,
}

fn invalid(field: &'static str, problem: Problem) -> Result<(), Invalid> {
    Err(Invalid { field, problem })
}

/// 1–64 bytes of printable ASCII (no space).
fn check_id(field: &'static str, id: &str) -> Result<(), Invalid> {
    if id.is_empty() {
        return invalid(field, Problem::Empty);
    }
    if id.len() > MAX_ID_LEN {
        return invalid(field, Problem::TooLong);
    }
    if !id.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
        return invalid(field, Problem::BadCharacters);
    }
    Ok(())
}

fn check_optional_id(field: &'static str, id: Option<&str>) -> Result<(), Invalid> {
    id.map_or(Ok(()), |id| check_id(field, id))
}

/// At most `max` characters, no control characters; empty only when allowed.
fn check_label(field: &'static str, text: &str, max: usize, empty: bool) -> Result<(), Invalid> {
    if text.is_empty() && !empty {
        return invalid(field, Problem::Empty);
    }
    if text.chars().count() > max {
        return invalid(field, Problem::TooLong);
    }
    if text.chars().any(char::is_control) {
        return invalid(field, Problem::BadCharacters);
    }
    Ok(())
}

/// Content: 1–4000 characters, anything goes.
fn check_text(field: &'static str, text: &str) -> Result<(), Invalid> {
    if text.is_empty() {
        return invalid(field, Problem::Empty);
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return invalid(field, Problem::TooLong);
    }
    Ok(())
}

/// A BCP 47 tag's shape: 1–35 of `[A-Za-z0-9-]`.
fn check_language(field: &'static str, tag: &str) -> Result<(), Invalid> {
    if tag.is_empty() {
        return invalid(field, Problem::Empty);
    }
    if tag.len() > MAX_LANGUAGE_LEN {
        return invalid(field, Problem::TooLong);
    }
    if !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return invalid(field, Problem::BadCharacters);
    }
    Ok(())
}

impl BridgeHello {
    fn validate(&self) -> Result<(), Invalid> {
        check_label("protocol", &self.protocol, MAX_NAME_CHARS, false)?;
        if self.protocols.len() > MAX_PROTOCOLS {
            return invalid("protocols", Problem::TooMany);
        }
        for protocol in &self.protocols {
            check_label("protocols", protocol, MAX_NAME_CHARS, false)?;
        }
        check_label("bridge_version", &self.bridge_version, MAX_NAME_CHARS, true)?;
        check_language("language", &self.language)?;
        check_label("audio.format", &self.audio.format, 16, false)
    }
}

impl AdapterHello {
    fn validate(&self) -> Result<(), Invalid> {
        check_label("protocol", &self.protocol, MAX_NAME_CHARS, false)?;
        check_label("name", &self.name, MAX_NAME_CHARS, false)?;
        check_label("version", &self.version, MAX_NAME_CHARS, true)?;
        if self.voices.len() > MAX_VOICES {
            return invalid("voices", Problem::TooMany);
        }
        for voice in &self.voices {
            check_label("voices.id", &voice.id, MAX_NAME_CHARS, false)?;
            if let Some(language) = &voice.language {
                check_language("voices.language", language)?;
            }
            if let Some(name) = &voice.name {
                check_label("voices.name", name, MAX_NAME_CHARS, true)?;
            }
        }
        Ok(())
    }
}

impl ErrorMessage {
    fn validate(&self) -> Result<(), Invalid> {
        if self.message.chars().count() > MAX_ERROR_MESSAGE_CHARS {
            return invalid("message", Problem::TooLong);
        }
        if self.message.chars().any(char::is_control) {
            return invalid("message", Problem::BadCharacters);
        }
        check_optional_id("call_id", self.call_id.as_deref())?;
        check_optional_id("speak_id", self.speak_id.as_deref())?;
        check_optional_id("utterance_id", self.utterance_id.as_deref())
    }
}

impl Message for BridgeMessage {
    const TYPES: &'static [&'static str] = &[
        "hello",
        "call_started",
        "speak",
        "stop_speaking",
        "call_ended",
        "error",
        "ping",
        "pong",
    ];

    fn message_type(&self) -> &'static str {
        match self {
            BridgeMessage::Hello(_) => "hello",
            BridgeMessage::CallStarted(_) => "call_started",
            BridgeMessage::Speak(_) => "speak",
            BridgeMessage::StopSpeaking(_) => "stop_speaking",
            BridgeMessage::CallEnded(_) => "call_ended",
            BridgeMessage::Error(_) => "error",
            BridgeMessage::Ping(_) => "ping",
            BridgeMessage::Pong(_) => "pong",
        }
    }

    fn validate(&self) -> Result<(), Invalid> {
        match self {
            BridgeMessage::Hello(hello) => hello.validate(),
            BridgeMessage::CallStarted(call) => {
                check_id("call_id", &call.call_id)?;
                check_label("peer", &call.peer, MAX_NAME_CHARS, true)?;
                check_language("language", &call.language)
            }
            BridgeMessage::Speak(speak) => {
                check_id("call_id", &speak.call_id)?;
                check_id("speak_id", &speak.speak_id)?;
                check_text("text", &speak.text)?;
                match &speak.voice {
                    Some(voice) => check_label("voice", voice, MAX_NAME_CHARS, false),
                    None => Ok(()),
                }
            }
            BridgeMessage::StopSpeaking(stop) => {
                check_id("call_id", &stop.call_id)?;
                check_optional_id("speak_id", stop.speak_id.as_deref())
            }
            BridgeMessage::CallEnded(ended) => check_id("call_id", &ended.call_id),
            BridgeMessage::Error(error) => error.validate(),
            BridgeMessage::Ping(_) | BridgeMessage::Pong(_) => Ok(()),
        }
    }
}

impl Message for AdapterMessage {
    const TYPES: &'static [&'static str] = &[
        "hello",
        "speech_started",
        "transcript",
        "speak_done",
        "error",
        "ping",
        "pong",
    ];

    fn message_type(&self) -> &'static str {
        match self {
            AdapterMessage::Hello(_) => "hello",
            AdapterMessage::SpeechStarted(_) => "speech_started",
            AdapterMessage::Transcript(_) => "transcript",
            AdapterMessage::SpeakDone(_) => "speak_done",
            AdapterMessage::Error(_) => "error",
            AdapterMessage::Ping(_) => "ping",
            AdapterMessage::Pong(_) => "pong",
        }
    }

    fn validate(&self) -> Result<(), Invalid> {
        match self {
            AdapterMessage::Hello(hello) => hello.validate(),
            AdapterMessage::SpeechStarted(started) => {
                check_id("call_id", &started.call_id)?;
                check_id("utterance_id", &started.utterance_id)
            }
            AdapterMessage::Transcript(transcript) => {
                check_id("call_id", &transcript.call_id)?;
                check_id("utterance_id", &transcript.utterance_id)?;
                check_text("text", &transcript.text)?;
                if let Some(confidence) = transcript.confidence {
                    if !(0.0..=1.0).contains(&confidence) {
                        return invalid("confidence", Problem::OutOfRange);
                    }
                }
                match &transcript.language {
                    Some(language) => check_language("language", language),
                    None => Ok(()),
                }
            }
            AdapterMessage::SpeakDone(done) => {
                check_id("call_id", &done.call_id)?;
                check_id("speak_id", &done.speak_id)
            }
            AdapterMessage::Error(error) => error.validate(),
            AdapterMessage::Ping(_) | AdapterMessage::Pong(_) => Ok(()),
        }
    }
}

/// A decoded control message: one this version knows, or the (sanitized) `type` of one
/// it does not, to be ignored.
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded<M> {
    Known(M),
    Unknown(String),
}

/// A control message that cannot be decoded or encoded. Content-free: it names the
/// message type, a missing field or a rule, never a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MessageError {
    #[error("control message is not valid JSON ({category} error at line {line}, column {column})")]
    Syntax {
        category: &'static str,
        line: usize,
        column: usize,
    },
    #[error("control message is not a JSON object")]
    NotAnObject,
    #[error("control message has no string `type`")]
    MissingType,
    #[error("`{message_type}` message does not have the expected fields{}", missing_field.as_deref().map(|f| format!(" (missing `{f}`)")).unwrap_or_default())]
    Shape {
        message_type: &'static str,
        missing_field: Option<String>,
    },
    #[error("`{message_type}` message: {invalid}")]
    Invalid {
        message_type: &'static str,
        invalid: Invalid,
    },
    #[error("control message of {len} bytes exceeds the frame limit of {max} bytes")]
    TooLarge { len: usize, max: usize },
}

/// The name of a missing field from serde's "missing field `name`" (our own field names
/// only: lowercase and `_`), never anything else from the error text.
fn missing_field(error: &serde_json::Error) -> Option<String> {
    let text = error.to_string();
    let rest = text.strip_prefix("missing field `")?;
    let name = &rest[..rest.find('`')?];
    let ours = !name.is_empty()
        && name.len() <= 32
        && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    ours.then(|| name.to_owned())
}

/// An unknown type's name, safe to log: at most 64 characters of `[a-z0-9_.-]`, others
/// replaced by `?`.
fn sanitize_type(name: &str) -> String {
    name.chars()
        .take(64)
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '_' | '.' | '-' => c,
            _ => '?',
        })
        .collect()
}

/// Decodes a control frame's JSON as a message of direction `M`: unknown fields are
/// ignored, an unknown `type` is [`Decoded::Unknown`], a known one is checked
/// ([`Message::validate`]).
pub fn decode<M: Message>(json: &[u8]) -> Result<Decoded<M>, MessageError> {
    let value: serde_json::Value =
        serde_json::from_slice(json).map_err(|error| MessageError::Syntax {
            category: match error.classify() {
                serde_json::error::Category::Io => "io",
                serde_json::error::Category::Syntax => "syntax",
                serde_json::error::Category::Data => "data",
                serde_json::error::Category::Eof => "eof",
            },
            line: error.line(),
            column: error.column(),
        })?;
    let object = value.as_object().ok_or(MessageError::NotAnObject)?;
    let type_name = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or(MessageError::MissingType)?;
    let known: &'static str = match M::TYPES.iter().find(|known| **known == type_name) {
        Some(known) => known,
        None => return Ok(Decoded::Unknown(sanitize_type(type_name))),
    };
    let message: M = serde_json::from_value(value).map_err(|error| MessageError::Shape {
        message_type: known,
        missing_field: missing_field(&error),
    })?;
    message
        .validate()
        .map_err(|invalid| MessageError::Invalid {
            message_type: known,
            invalid,
        })?;
    Ok(Decoded::Known(message))
}

/// Validates and encodes a message as a control frame.
pub fn encode<M: Message>(message: &M) -> Result<Frame, MessageError> {
    let message_type = message.message_type();
    message
        .validate()
        .map_err(|invalid| MessageError::Invalid {
            message_type,
            invalid,
        })?;
    let json = serde_json::to_vec(message).map_err(|_| MessageError::Shape {
        message_type,
        missing_field: None,
    })?;
    if json.len() > MAX_PAYLOAD {
        return Err(MessageError::TooLarge {
            len: json.len(),
            max: MAX_PAYLOAD,
        });
    }
    Ok(Frame::Control(json))
}
