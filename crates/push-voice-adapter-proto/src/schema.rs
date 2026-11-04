//! The JSON Schemas (draft 2020-12) of the control messages, generated from the Rust
//! types. They are checked in under [`DIR`]; a test regenerates them and fails on any
//! difference (`UPDATE_VOICE_ADAPTER_SCHEMAS=1` rewrites them).
//!
//! One file per message (with its `type` as a constant) and one union per direction.
//! Receivers must still ignore unknown fields and message types: the schemas describe
//! what a v1 sender sends.

use schemars::JsonSchema;
use serde_json::{Value, json};

use crate::message::{
    AdapterHello, AdapterMessage, BridgeHello, BridgeMessage, CallEnded, CallStarted, ErrorMessage,
    Ping, Pong, Speak, SpeakDone, SpeechStarted, StopSpeaking, Transcript,
};

/// Where the schemas live, from the repository root.
pub const DIR: &str = "docs/schemas/voice-adapter/v1";

const BASE: &str = "https://raw.githubusercontent.com/yury-opolev/push-messenger/main/docs/schemas/voice-adapter/v1/";

/// Every schema file: its name and content.
pub fn schemas() -> Vec<(&'static str, Value)> {
    vec![
        (
            "bridge-message.json",
            union::<BridgeMessage>("bridge-message.json"),
        ),
        (
            "adapter-message.json",
            union::<AdapterMessage>("adapter-message.json"),
        ),
        (
            "bridge-hello.json",
            message::<BridgeHello>("bridge-hello.json", "hello"),
        ),
        (
            "adapter-hello.json",
            message::<AdapterHello>("adapter-hello.json", "hello"),
        ),
        (
            "call-started.json",
            message::<CallStarted>("call-started.json", "call_started"),
        ),
        ("speak.json", message::<Speak>("speak.json", "speak")),
        (
            "stop-speaking.json",
            message::<StopSpeaking>("stop-speaking.json", "stop_speaking"),
        ),
        (
            "call-ended.json",
            message::<CallEnded>("call-ended.json", "call_ended"),
        ),
        (
            "speech-started.json",
            message::<SpeechStarted>("speech-started.json", "speech_started"),
        ),
        (
            "transcript.json",
            message::<Transcript>("transcript.json", "transcript"),
        ),
        (
            "speak-done.json",
            message::<SpeakDone>("speak-done.json", "speak_done"),
        ),
        ("error.json", message::<ErrorMessage>("error.json", "error")),
        ("ping.json", message::<Ping>("ping.json", "ping")),
        ("pong.json", message::<Pong>("pong.json", "pong")),
    ]
}

/// The file's text: pretty JSON and a final newline.
pub fn render(schema: &Value) -> String {
    let mut text = serde_json::to_string_pretty(schema).expect("a JSON value always renders");
    text.push('\n');
    text
}

fn union<T: JsonSchema>(file: &str) -> Value {
    let mut schema = schemars::schema_for!(T).to_value();
    if let Some(object) = schema.as_object_mut() {
        object.insert("$id".into(), json!(format!("{BASE}{file}")));
    }
    schema
}

fn message<T: JsonSchema>(file: &str, type_name: &str) -> Value {
    let mut schema = schemars::schema_for!(T).to_value();
    if let Some(object) = schema.as_object_mut() {
        object.insert("$id".into(), json!(format!("{BASE}{file}")));
        if let Some(Value::Object(properties)) = object.get_mut("properties") {
            properties.insert(
                "type".into(),
                json!({ "type": "string", "const": type_name }),
            );
        }
        let required = object
            .entry("required")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(required) = required {
            required.insert(0, json!("type"));
        }
    }
    schema
}
