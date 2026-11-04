//! Example messages of both directions with their expected wire JSON.

use push_voice_adapter_proto::{
    AdapterHello, AdapterMessage, BridgeHello, BridgeMessage, CallEndReason, CallEnded,
    CallStarted, Capabilities, ErrorCode, ErrorMessage, Ping, Pong, Speak, SpeakDone,
    SpeechStarted, StopSpeaking, Transcript, Voice,
};
use serde_json::{Value, json};

pub fn bridge_messages() -> Vec<(BridgeMessage, Value)> {
    vec![
        (
            BridgeMessage::Hello(BridgeHello::v1("0.1.0", "en-US")),
            json!({"type": "hello", "protocol": "push-voice-adapter/1",
                   "protocols": ["push-voice-adapter/1"], "bridge_version": "0.1.0",
                   "language": "en-US",
                   "audio": {"rate": 16000, "channels": 1, "format": "s16le", "frame_ms": 20}}),
        ),
        (
            BridgeMessage::CallStarted(CallStarted {
                call_id: "c1".into(),
                peer: "Yury".into(),
                language: "en-US".into(),
            }),
            json!({"type": "call_started", "call_id": "c1", "peer": "Yury", "language": "en-US"}),
        ),
        (
            BridgeMessage::Speak(Speak {
                call_id: "c1".into(),
                speak_id: "s1".into(),
                text: "Hello there.".into(),
                interruptible: true,
                voice: Some("en-US-JennyNeural".into()),
            }),
            json!({"type": "speak", "call_id": "c1", "speak_id": "s1", "text": "Hello there.",
                   "interruptible": true, "voice": "en-US-JennyNeural"}),
        ),
        (
            BridgeMessage::StopSpeaking(StopSpeaking {
                call_id: "c1".into(),
                speak_id: None,
            }),
            json!({"type": "stop_speaking", "call_id": "c1"}),
        ),
        (
            BridgeMessage::CallEnded(CallEnded {
                call_id: "c1".into(),
                reason: CallEndReason::Completed,
            }),
            json!({"type": "call_ended", "call_id": "c1", "reason": "completed"}),
        ),
        (
            BridgeMessage::Error(
                ErrorMessage::new(ErrorCode::UnsupportedProtocol, "speaks /1 only").for_call("c1"),
            ),
            json!({"type": "error", "code": "unsupported_protocol", "message": "speaks /1 only",
                   "call_id": "c1"}),
        ),
        (
            BridgeMessage::Ping(Ping { id: 1 }),
            json!({"type": "ping", "id": 1}),
        ),
        (
            BridgeMessage::Pong(Pong { id: u32::MAX }),
            json!({"type": "pong", "id": 4294967295u32}),
        ),
    ]
}

pub fn adapter_messages() -> Vec<(AdapterMessage, Value)> {
    let mut hello = AdapterHello::new(
        "test-adapter",
        "1.2.3",
        Capabilities {
            stt: true,
            tts: true,
            vad: true,
            barge_in: false,
            partials: true,
        },
    );
    hello.voices.push(Voice {
        id: "jenny".into(),
        language: Some("en-US".into()),
        name: None,
    });
    vec![
        (
            AdapterMessage::Hello(hello),
            json!({"type": "hello", "protocol": "push-voice-adapter/1", "name": "test-adapter",
                   "version": "1.2.3",
                   "capabilities": {"stt": true, "tts": true, "vad": true, "barge_in": false,
                                    "partials": true},
                   "voices": [{"id": "jenny", "language": "en-US"}]}),
        ),
        (
            AdapterMessage::SpeechStarted(SpeechStarted {
                call_id: "c1".into(),
                utterance_id: "u1".into(),
            }),
            json!({"type": "speech_started", "call_id": "c1", "utterance_id": "u1"}),
        ),
        (
            AdapterMessage::Transcript(Transcript {
                call_id: "c1".into(),
                utterance_id: "u1".into(),
                text: "approve it".into(),
                is_final: true,
                confidence: Some(0.5),
                language: Some("en-US".into()),
            }),
            json!({"type": "transcript", "call_id": "c1", "utterance_id": "u1",
                   "text": "approve it", "final": true, "confidence": 0.5, "language": "en-US"}),
        ),
        (
            AdapterMessage::SpeakDone(SpeakDone {
                call_id: "c1".into(),
                speak_id: "s1".into(),
                interrupted: true,
            }),
            json!({"type": "speak_done", "call_id": "c1", "speak_id": "s1", "interrupted": true}),
        ),
        (
            AdapterMessage::Error(ErrorMessage {
                speak_id: Some("s1".into()),
                ..ErrorMessage::new(ErrorCode::SynthesisFailed, "engine timed out").for_call("c1")
            }),
            json!({"type": "error", "code": "synthesis_failed", "message": "engine timed out",
                   "call_id": "c1", "speak_id": "s1"}),
        ),
        (
            AdapterMessage::Ping(Ping { id: 0 }),
            json!({"type": "ping", "id": 0}),
        ),
        (
            AdapterMessage::Pong(Pong { id: 7 }),
            json!({"type": "pong", "id": 7}),
        ),
    ]
}
