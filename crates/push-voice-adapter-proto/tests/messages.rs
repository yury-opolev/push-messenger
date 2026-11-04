//! Control messages: the wire shape of each, round trips, tolerance of unknown fields and
//! types, every cap, and content-free `Debug` and errors.

mod common;

use common::{adapter_messages, bridge_messages};
use push_voice_adapter_proto::message::Problem;
use push_voice_adapter_proto::{
    AdapterHello, AdapterMessage, BridgeHello, BridgeMessage, CallEndReason, CallEnded,
    CallStarted, Capabilities, Decoded, ErrorCode, ErrorMessage, Frame, Invalid, Message,
    MessageError, PROTOCOL, Speak, StopSpeaking, Transcript, Voice, decode, encode, negotiate,
};
use serde_json::{Value, json};

fn wire<M: Message>(message: &M) -> Value {
    let Frame::Control(json) = encode(message).unwrap() else {
        panic!("a control frame");
    };
    serde_json::from_slice(&json).unwrap()
}

fn known<M: Message + std::fmt::Debug>(value: Value) -> M {
    match decode::<M>(value.to_string().as_bytes()) {
        Ok(Decoded::Known(message)) => message,
        other => panic!("{other:?}"),
    }
}

fn round_trip<M: Message + PartialEq + std::fmt::Debug + Clone>(message: M, expected: Value) {
    assert_eq!(wire(&message), expected);
    assert_eq!(known::<M>(expected), message);
}

#[test]
fn every_message_has_its_documented_wire_shape_and_round_trips() {
    let bridge = bridge_messages();
    let adapter = adapter_messages();
    assert_eq!(bridge.len(), BridgeMessage::TYPES.len());
    assert_eq!(adapter.len(), AdapterMessage::TYPES.len());
    for (message, expected) in bridge {
        assert_eq!(expected["type"], message.message_type());
        round_trip(message, expected);
    }
    for (message, expected) in adapter {
        assert_eq!(expected["type"], message.message_type());
        round_trip(message, expected);
    }
}

#[test]
fn unknown_fields_are_ignored_and_optional_ones_default() {
    let message: AdapterMessage = known(json!({
        "type": "hello", "protocol": "push-voice-adapter/1", "name": "a", "version": "",
        "capabilities": {"stt": true, "telepathy": true}, "future": {"nested": [1, 2]}
    }));
    let AdapterMessage::Hello(hello) = message else {
        panic!()
    };
    assert_eq!(
        hello.capabilities,
        Capabilities {
            stt: true,
            ..Capabilities::default()
        }
    );
    assert!(hello.voices.is_empty());
    let message: BridgeMessage = known(json!({
        "type": "stop_speaking", "call_id": "c", "speak_id": null, "extra": 1
    }));
    assert_eq!(
        message,
        BridgeMessage::StopSpeaking(StopSpeaking {
            call_id: "c".into(),
            speak_id: None
        })
    );
    let error: AdapterMessage = known(json!({"type": "error", "code": "engine_unavailable"}));
    assert_eq!(
        error,
        AdapterMessage::Error(ErrorMessage::new(ErrorCode::EngineUnavailable, ""))
    );
}

#[test]
fn unknown_types_and_enum_values_are_tolerated() {
    assert_eq!(
        decode::<AdapterMessage>(br#"{"type":"emotion","call_id":"c","mood":"happy"}"#),
        Ok(Decoded::Unknown("emotion".into()))
    );
    // A bridge-only type is unknown to the bridge's decoder of adapter messages.
    assert_eq!(
        decode::<AdapterMessage>(br#"{"type":"speak"}"#),
        Ok(Decoded::Unknown("speak".into()))
    );
    // The name is sanitized for logs.
    assert_eq!(
        decode::<AdapterMessage>(br#"{"type":"Bad Type\n"}"#),
        Ok(Decoded::Unknown("?ad??ype?".into()))
    );
    let long = format!(r#"{{"type":"{}"}}"#, "x".repeat(500));
    let Ok(Decoded::Unknown(name)) = decode::<BridgeMessage>(long.as_bytes()) else {
        panic!()
    };
    assert_eq!(name.len(), 64);
    let error: BridgeMessage = known(json!({"type": "error", "code": "quota_exceeded"}));
    assert_eq!(
        error,
        BridgeMessage::Error(ErrorMessage::new(ErrorCode::Other, ""))
    );
    let ended: BridgeMessage =
        known(json!({"type": "call_ended", "call_id": "c", "reason": "transferred"}));
    assert_eq!(
        ended,
        BridgeMessage::CallEnded(CallEnded {
            call_id: "c".into(),
            reason: CallEndReason::Other
        })
    );
}

#[test]
fn malformed_messages_are_content_free_errors() {
    let secret = "my password is hunter2";
    let cases: Vec<(String, MessageError)> = vec![
        (
            format!("{{\"type\":\"transcript\",\"text\":\"{secret}\""),
            MessageError::Syntax {
                category: "eof",
                line: 1,
                column: 52,
            },
        ),
        (format!("[\"{secret}\"]"), MessageError::NotAnObject),
        (
            format!("{{\"text\":\"{secret}\"}}"),
            MessageError::MissingType,
        ),
        (
            format!("{{\"type\":7,\"text\":\"{secret}\"}}"),
            MessageError::MissingType,
        ),
        (
            format!(
                r#"{{"type":"transcript","call_id":"c","utterance_id":"u","text":"{secret}"}}"#
            ),
            MessageError::Shape {
                message_type: "transcript",
                missing_field: Some("final".into()),
            },
        ),
        (
            format!(
                r#"{{"type":"transcript","call_id":"c","utterance_id":"u","text":"{secret}","final":"{secret}"}}"#
            ),
            MessageError::Shape {
                message_type: "transcript",
                missing_field: None,
            },
        ),
        (
            format!(
                r#"{{"type":"transcript","call_id":"{secret}","utterance_id":"u","text":"x","final":true}}"#
            ),
            MessageError::Invalid {
                message_type: "transcript",
                invalid: Invalid {
                    field: "call_id",
                    problem: Problem::BadCharacters,
                },
            },
        ),
    ];
    for (json, expected) in cases {
        let error = decode::<AdapterMessage>(json.as_bytes()).unwrap_err();
        assert_eq!(error, expected, "{json}");
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
    }
}

#[test]
fn every_cap_is_enforced_on_decode_and_encode() {
    fn transcript(text: String, confidence: Option<f32>) -> AdapterMessage {
        AdapterMessage::Transcript(Transcript {
            call_id: "c".into(),
            utterance_id: "u".into(),
            text,
            is_final: true,
            confidence,
            language: None,
        })
    }
    fn speak(call_id: &str, text: String) -> BridgeMessage {
        BridgeMessage::Speak(Speak {
            call_id: call_id.into(),
            speak_id: "s".into(),
            text,
            interruptible: true,
            voice: None,
        })
    }
    fn rejected<M: Message>(message: &M, field: &'static str, problem: Problem) {
        let expected = Invalid { field, problem };
        assert_eq!(message.validate(), Err(expected));
        assert!(
            matches!(encode(message), Err(MessageError::Invalid { invalid, .. }) if invalid == expected)
        );
        let json = serde_json::to_vec(message).unwrap();
        assert!(
            matches!(decode::<M>(&json), Err(MessageError::Invalid { invalid, .. }) if invalid == expected)
        );
    }

    // 4000 characters (multi-byte ones count once) pass, 4001 fail, empty fails.
    assert!(transcript("é".repeat(4000), None).validate().is_ok());
    rejected(
        &transcript("é".repeat(4001), None),
        "text",
        Problem::TooLong,
    );
    rejected(&transcript(String::new(), None), "text", Problem::Empty);
    assert!(speak("c", "x".repeat(4000)).validate().is_ok());
    rejected(&speak("c", "x".repeat(4001)), "text", Problem::TooLong);
    // Confidence is 0 to 1.
    assert!(transcript("x".into(), Some(1.0)).validate().is_ok());
    rejected(
        &transcript("x".into(), Some(1.5)),
        "confidence",
        Problem::OutOfRange,
    );
    rejected(
        &transcript("x".into(), Some(-0.1)),
        "confidence",
        Problem::OutOfRange,
    );
    // Ids: 1–64 printable ASCII.
    assert!(speak(&"a".repeat(64), "x".into()).validate().is_ok());
    rejected(
        &speak(&"a".repeat(65), "x".into()),
        "call_id",
        Problem::TooLong,
    );
    rejected(&speak("", "x".into()), "call_id", Problem::Empty);
    rejected(&speak("a b", "x".into()), "call_id", Problem::BadCharacters);
    rejected(&speak("ä", "x".into()), "call_id", Problem::BadCharacters);
    // Languages, names, versions, voices and error messages.
    rejected(
        &BridgeMessage::Hello(BridgeHello::v1("1", "en_US")),
        "language",
        Problem::BadCharacters,
    );
    rejected(
        &BridgeMessage::Hello(BridgeHello::v1("1", "x".repeat(36))),
        "language",
        Problem::TooLong,
    );
    rejected(
        &BridgeMessage::Hello(BridgeHello::v1("1\n", "en")),
        "bridge_version",
        Problem::BadCharacters,
    );
    let mut hello = AdapterHello::new("", "1", Capabilities::default());
    rejected(
        &AdapterMessage::Hello(hello.clone()),
        "name",
        Problem::Empty,
    );
    hello.name = "n".repeat(65);
    rejected(
        &AdapterMessage::Hello(hello.clone()),
        "name",
        Problem::TooLong,
    );
    hello.name = "n".into();
    hello.voices = (0..257)
        .map(|i| Voice {
            id: format!("v{i}"),
            language: None,
            name: None,
        })
        .collect();
    rejected(&AdapterMessage::Hello(hello), "voices", Problem::TooMany);
    rejected(
        &AdapterMessage::Error(ErrorMessage::new(ErrorCode::Internal, "e".repeat(513))),
        "message",
        Problem::TooLong,
    );
    rejected(
        &AdapterMessage::Error(ErrorMessage::new(ErrorCode::Internal, "line\r\nforged")),
        "message",
        Problem::BadCharacters,
    );
    rejected(
        &BridgeMessage::CallStarted(CallStarted {
            call_id: "c".into(),
            peer: "p".repeat(65),
            language: "en".into(),
        }),
        "peer",
        Problem::TooLong,
    );
}

#[test]
fn an_encoded_message_over_64_kib_is_refused() {
    let mut hello = AdapterHello::new("a", "1", Capabilities::default());
    hello.voices = (0..256)
        .map(|i| Voice {
            id: format!("{i:0>64}"),
            language: Some("x".repeat(35)),
            name: Some("ü".repeat(64)),
        })
        .collect();
    assert!(matches!(
        encode(&AdapterMessage::Hello(hello)),
        Err(MessageError::TooLarge { .. })
    ));
}

#[test]
fn debug_never_shows_content() {
    let (speak, _) = bridge_messages().swap_remove(2);
    let (started, _) = bridge_messages().swap_remove(1);
    let (transcript, _) = adapter_messages().swap_remove(2);
    let error = AdapterMessage::Error(ErrorMessage::new(ErrorCode::Internal, "secret detail"));
    let shown = format!("{speak:?} {started:?} {transcript:?} {error:?}");
    for content in ["Hello there", "Yury", "approve it", "secret detail"] {
        assert!(!shown.contains(content), "{content} in {shown}");
    }
    assert!(shown.contains("<12 chars>"), "{shown}");
    assert!(
        shown.contains("s1") && shown.contains("u1"),
        "ids are shown: {shown}"
    );
}

#[test]
fn hellos_negotiate_the_version() {
    let hello = BridgeHello::v1("1", "en-US");
    assert_eq!(hello.offered(), vec![PROTOCOL]);
    assert_eq!(
        negotiate(hello.offered(), push_voice_adapter_proto::SUPPORTED),
        Some(PROTOCOL.to_owned())
    );
    // An older bridge that sends `protocol` only.
    let old: BridgeMessage = known(json!({
        "type": "hello", "protocol": "push-voice-adapter/1", "bridge_version": "0",
        "language": "en", "audio": {"rate": 16000, "channels": 1, "format": "s16le", "frame_ms": 20}
    }));
    let BridgeMessage::Hello(old) = old else {
        panic!()
    };
    assert_eq!(old.offered(), vec![PROTOCOL]);
    // A future bridge offering /2 and /1 gets /1 from this crate.
    let mut future = hello;
    future.protocol = "push-voice-adapter/2".into();
    future.protocols = vec!["push-voice-adapter/2".into(), PROTOCOL.into()];
    assert_eq!(
        negotiate(future.offered(), push_voice_adapter_proto::SUPPORTED),
        Some(PROTOCOL.to_owned())
    );
    future.protocols.pop();
    assert_eq!(
        negotiate(future.offered(), push_voice_adapter_proto::SUPPORTED),
        None
    );
}
