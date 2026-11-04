//! The wire format: newline-delimited JSON, one UTF-8 object per line, at most
//! [`MAX_LINE`] bytes. Clients send [`Request`]s; the bridge sends [`ServerLine`]s, a
//! [`Response`] per request and [`EventLine`]s as things happen. Everything here is
//! content-free (states, counts, names and error codes, never message text) except the
//! `peek` result ([`PeekResult`]), which carries the owner conversation's last messages,
//! masked and capped, and only when the bridge's config allows it.

use serde::de::Error as _;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// The protocol version in `hello` and `status`.
pub const PROTOCOL: u32 = 1;
/// The longest line either side accepts, in bytes.
pub const MAX_LINE: usize = 64 * 1024;
/// The most entries a bridge keeps and a `peek` returns.
pub const PEEK_CAP: usize = 50;
/// Entries a `peek` without `limit` returns.
pub const PEEK_DEFAULT: usize = 20;
/// The longest entry text, in characters.
pub const PEEK_TEXT_MAX: usize = 300;
/// The error code of a `peek` when the bridge's config.json has `"peek": false`.
pub const PEEK_DISABLED: &str = "peek_disabled";

/// A client's request line: `{"id":1,"method":"status","params":{}}`.
///
/// Serialized as `{"id","method"}` plus `"params"` for calls that have them; a call
/// without parameters accepts absent, `null` or object `params`.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: u64,
    pub call: Call,
}

impl Serialize for Request {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("method", self.call.method())?;
        match &self.call {
            Call::PauseBackend(params) => map.serialize_entry("params", params)?,
            Call::Peek(params) if params.limit.is_some() => {
                map.serialize_entry("params", params)?;
            }
            Call::Mute(params) if params.muted.is_some() => {
                map.serialize_entry("params", params)?;
            }
            _ => {}
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        request_from_value(value)
            .map_err(|(_, error)| D::Error::custom(format!("{}: {}", error.code, error.message)))
    }
}

/// What a request asks for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Call {
    Status,
    RestartBackend,
    Reconnect,
    Shutdown,
    PauseBackend(PauseParams),
    ResumeBackend,
    /// The owner conversation's last messages; see [`PeekResult`].
    Peek(PeekParams),
    /// The voice call's state: `{"call": CallInfo | null}`, from the snapshot.
    CallState,
    /// Mutes (or unmutes) the agent's voice in the running call.
    Mute(MuteParams),
    /// Hangs up the running call, cancels a call ringing the owner, or declines a call
    /// waiting for the owner's confirmation.
    Hangup,
}

impl Call {
    /// The method's wire name.
    pub fn method(&self) -> &'static str {
        match self {
            Call::Status => "status",
            Call::RestartBackend => "restart_backend",
            Call::Reconnect => "reconnect",
            Call::Shutdown => "shutdown",
            Call::PauseBackend(_) => "pause_backend",
            Call::ResumeBackend => "resume_backend",
            Call::Peek(_) => "peek",
            Call::CallState => "call_state",
            Call::Mute(_) => "mute",
            Call::Hangup => "hangup",
        }
    }
}

/// `mute`'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MuteParams {
    /// `true` (the default) mutes the agent's voice, `false` unmutes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}

/// The error code of `mute` and `hangup` when no call runs.
pub const NO_CALL: &str = "no_call";

/// Where a voice call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallPhase {
    /// The owner's call waits for the owner's confirmation (`calls.answer` `ask`), or the
    /// bridge's call rings the owner.
    Ringing,
    /// Answered (or accepted by the owner); the audio is being connected.
    Connecting,
    /// Audio flows.
    InCall,
}

/// Who started a voice call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallDirection {
    /// The owner called the agent.
    Incoming,
    /// The agent called the owner (`notify_by_call`).
    Outgoing,
}

/// The voice call's state: content-free (a phase, a direction, flags and a time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallInfo {
    pub phase: CallPhase,
    pub direction: CallDirection,
    /// The agent's voice plays.
    #[serde(default)]
    pub speaking: bool,
    /// The owner speaks.
    #[serde(default)]
    pub listening: bool,
    /// The agent's voice is muted (`mute`).
    #[serde(default)]
    pub muted: bool,
    /// When the call entered its phase, Unix ms.
    pub since: i64,
}

/// The `call_state` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CallStateResult {
    pub call: Option<CallInfo>,
}

/// `peek`'s parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PeekParams {
    /// How many entries; see [`peek_limit`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// The number of entries a peek returns: `limit` (default [`PEEK_DEFAULT`]) clamped to
/// `1..=PEEK_CAP`.
pub fn peek_limit(params: &PeekParams) -> usize {
    params.limit.unwrap_or(PEEK_DEFAULT).clamp(1, PEEK_CAP)
}

/// One message in the peek.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeekEntry {
    /// Unix milliseconds.
    pub time: i64,
    pub from: PeekFrom,
    pub kind: PeekKind,
    /// Masked as sent, at most [`PEEK_TEXT_MAX`] characters.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_state: Option<CardState>,
}

/// Who sent a peek entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeekFrom {
    Owner,
    Agent,
}

/// What a peek entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeekKind {
    Text,
    Reply,
    Card,
    Answer,
    Step,
    Note,
}

/// A card entry's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardState {
    Open,
    Answered,
    Declined,
    Expired,
}

/// The `peek` result: oldest first (newest last).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PeekResult {
    pub entries: Vec<PeekEntry>,
}

/// `pause_backend`'s parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PauseParams {
    /// Why, for people; the server caps it to 100 chars.
    #[serde(default)]
    pub reason: Option<String>,
    /// How long the backend may drain; the server's default is 600 000.
    #[serde(default)]
    pub drain_timeout_ms: Option<u64>,
}

/// A server → client line: a response or an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerLine {
    Response(Response),
    Event(EventLine),
}

/// The answer to the request with the same `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

/// A failed request's error: a stable code and a message for people.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

/// An event with its time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventLine {
    #[serde(flatten)]
    pub event: Event,
    /// Unix milliseconds.
    pub at: i64,
}

/// Something that happened in the bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The first line on every connection.
    Hello {
        protocol: u32,
        bridge: String,
        pid: u32,
        instance: String,
    },
    StateChanged {
        state: BridgeState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// `source`: "prompt" | "engine".
    TurnStarted {
        source: String,
    },
    TurnEnded {
        interrupted: bool,
    },
    /// `kind`: "permission" | "question" | "plan" | "bypass".
    RequestOpened {
        kind: String,
        open: usize,
    },
    /// `outcome`: "answered" | "declined" | "expired".
    RequestClosed {
        kind: String,
        outcome: String,
        open: usize,
    },
    Error {
        code: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
    },
    /// The peek's entries changed (content-free: no entry in it).
    PeekChanged,
    /// The voice call started, changed phase or flags, or ended (`call` absent).
    CallChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<CallInfo>,
    },
    /// The voice adapter's state changed (its `metrics` are only in the status).
    AdapterChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        adapter: Option<AdapterStatus>,
    },
}

/// The bridge's state, as the tray shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeState {
    /// The process is up, signing in or opening the conversation.
    Starting,
    /// Connected, the backend runs, nothing to do.
    Idle,
    /// A turn or a slot command runs.
    Working,
    /// A card is open: waiting for the owner.
    Waiting,
    /// The messenger connection is down (the SDK reconnects) or the bridge signs in again.
    Reconnecting,
    /// The backend is down and restarts after the backoff.
    Restarting,
    /// Upgrade mode: draining, released, or waiting for the backend's executable.
    Paused,
    Stopping,
}

/// The `status` result. Every field is content-free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Status {
    pub protocol: u32,
    pub instance: String,
    pub pid: u32,
    pub bridge_version: String,
    /// Never `None` on the wire once started; `None` is the default for tests.
    pub state: Option<BridgeState>,
    pub detail: Option<String>,
    /// The agent's display name and handle once signed in.
    pub agent: Option<AgentInfo>,
    /// "connected" | "connecting" | "offline" | …
    pub connection: Option<String>,
    pub folder: Option<String>,
    pub session: Option<String>,
    pub backend: Option<BackendInfo>,
    pub open_requests: usize,
    /// Texts held during a pause (a count only).
    pub held_texts: usize,
    pub pause: Option<PauseInfo>,
    pub last_error: Option<LastError>,
    pub started_at: i64,
    /// The voice call, while one runs or rings.
    pub call: Option<CallInfo>,
    /// The voice adapter (`calls.adapter`); absent without one (and from older bridges).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<AdapterStatus>,
}

/// The voice adapter's state: content-free (a state, the adapter's name, version and
/// capabilities from its hello, a fixed reason, counts and durations).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct AdapterStatus {
    /// "off" | "starting" | "handshaking" | "ready" | "backoff".
    pub state: String,
    /// Ready: the adapter's name and version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Ready: what it can do: "stt", "tts", "vad", "barge_in", "partials".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    /// Backoff: why the last attempt ended (a short fixed phrase or an error code).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Backoff: when the adapter is started again, Unix ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<i64>,
    /// The counters since the bridge started (only in the status, not in
    /// `adapter_changed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<AdapterMetrics>,
}

/// The voice adapter's counters: frames, drops, restarts and latencies only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct AdapterMetrics {
    /// Calls the adapter took.
    pub calls: u64,
    /// The owner's audio frames given to the adapter.
    pub frames_to_adapter: u64,
    /// The adapter's audio frames played in calls.
    pub frames_from_adapter: u64,
    /// The owner's frames dropped because the adapter did not keep up (the current
    /// connection).
    pub dropped_to_adapter: u64,
    /// The adapter's frames dropped (too many, too fast, of a stopped or no speech).
    pub dropped_from_adapter: u64,
    /// How often the adapter was started again.
    pub restarts: u64,
    /// `speak` requests sent.
    pub speaks: u64,
    /// A final transcript → the first `speak` after it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_to_speak: Option<LatencyInfo>,
    /// A `speak` → its first audio frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speak_to_audio: Option<LatencyInfo>,
}

/// The last, average and longest of a latency, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct LatencyInfo {
    pub last_ms: u64,
    pub avg_ms: u64,
    pub max_ms: u64,
}

/// Who the bridge is signed in as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub display_name: String,
    pub handle: String,
}

/// The backend (Coda) the bridge drives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendInfo {
    pub name: String,
    pub version: Option<String>,
    pub running: bool,
    pub running_tasks: usize,
}

/// Upgrade mode's progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PauseInfo {
    /// "draining" | "released" | "waiting_for_file" | "resuming".
    pub phase: String,
    pub reason: Option<String>,
    pub since: i64,
}

/// The last error the bridge reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastError {
    pub code: String,
    pub at: i64,
}

/// Now, in Unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// A line to send: the JSON of `v` and `\n`.
pub fn encode<T: Serialize>(v: &T) -> String {
    let mut line = serde_json::to_string(v).expect("control-channel values serialize to JSON");
    line.push('\n');
    line
}

/// Parses a request line. On failure, the error carries the request's id when it was
/// readable, so the server can still answer: `bad_request` or `unknown_method`.
pub fn parse_request(line: &str) -> Result<Request, (Option<u64>, ErrorBody)> {
    if line.len() > MAX_LINE {
        return Err(bad_request(None, "the line is too long"));
    }
    let value = serde_json::from_str::<Value>(line)
        .map_err(|_| bad_request(None, "the line is not JSON"))?;
    request_from_value(value)
}

fn request_from_value(value: Value) -> Result<Request, (Option<u64>, ErrorBody)> {
    let Value::Object(mut object) = value else {
        return Err(bad_request(None, "a request is a JSON object"));
    };
    let Some(id) = object.get("id").and_then(Value::as_u64) else {
        return Err(bad_request(None, "id must be a non-negative integer"));
    };
    let Some(method) = object
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Err(bad_request(Some(id), "method must be a string"));
    };
    let params = match object.remove("params") {
        None | Some(Value::Null) => None,
        Some(params) => Some(params),
    };
    let unit = |call: Call| match &params {
        None | Some(Value::Object(_)) => Ok(Request { id, call }),
        Some(_) => Err(bad_request(Some(id), "params must be an object")),
    };
    match method.as_str() {
        "status" => unit(Call::Status),
        "restart_backend" => unit(Call::RestartBackend),
        "reconnect" => unit(Call::Reconnect),
        "shutdown" => unit(Call::Shutdown),
        "resume_backend" => unit(Call::ResumeBackend),
        "pause_backend" => {
            let params = match params {
                None => PauseParams::default(),
                Some(params) => serde_json::from_value(params)
                    .map_err(|_| bad_request(Some(id), "bad pause_backend params"))?,
            };
            Ok(Request {
                id,
                call: Call::PauseBackend(params),
            })
        }
        "peek" => {
            let params = match params {
                None => PeekParams::default(),
                Some(params @ Value::Object(_)) => serde_json::from_value(params)
                    .map_err(|_| bad_request(Some(id), "bad peek params"))?,
                Some(_) => return Err(bad_request(Some(id), "params must be an object")),
            };
            Ok(Request {
                id,
                call: Call::Peek(params),
            })
        }
        "call_state" => unit(Call::CallState),
        "hangup" => unit(Call::Hangup),
        "mute" => {
            let params = match params {
                None => MuteParams::default(),
                Some(params @ Value::Object(_)) => serde_json::from_value(params)
                    .map_err(|_| bad_request(Some(id), "bad mute params"))?,
                Some(_) => return Err(bad_request(Some(id), "params must be an object")),
            };
            Ok(Request {
                id,
                call: Call::Mute(params),
            })
        }
        _ => Err((
            Some(id),
            ErrorBody {
                code: "unknown_method".into(),
                message: format!("unknown method {method:?}"),
            },
        )),
    }
}

fn bad_request(id: Option<u64>, message: &str) -> (Option<u64>, ErrorBody) {
    (
        id,
        ErrorBody {
            code: "bad_request".into(),
            message: message.into(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(id: u64, call: Call) -> Request {
        Request { id, call }
    }

    #[test]
    fn every_call_round_trips_with_its_spelling() {
        let cases = [
            (Call::Status, r#"{"id":7,"method":"status"}"#),
            (
                Call::RestartBackend,
                r#"{"id":7,"method":"restart_backend"}"#,
            ),
            (Call::Reconnect, r#"{"id":7,"method":"reconnect"}"#),
            (Call::Shutdown, r#"{"id":7,"method":"shutdown"}"#),
            (Call::ResumeBackend, r#"{"id":7,"method":"resume_backend"}"#),
            (
                Call::PauseBackend(PauseParams {
                    reason: Some("x".into()),
                    drain_timeout_ms: Some(5),
                }),
                r#"{"id":7,"method":"pause_backend","params":{"reason":"x","drainTimeoutMs":5}}"#,
            ),
            (
                Call::Peek(PeekParams::default()),
                r#"{"id":7,"method":"peek"}"#,
            ),
            (
                Call::Peek(PeekParams { limit: Some(5) }),
                r#"{"id":7,"method":"peek","params":{"limit":5}}"#,
            ),
            (Call::CallState, r#"{"id":7,"method":"call_state"}"#),
            (Call::Hangup, r#"{"id":7,"method":"hangup"}"#),
            (
                Call::Mute(MuteParams::default()),
                r#"{"id":7,"method":"mute"}"#,
            ),
            (
                Call::Mute(MuteParams { muted: Some(false) }),
                r#"{"id":7,"method":"mute","params":{"muted":false}}"#,
            ),
        ];
        for (call, wire) in cases {
            let request = req(7, call);
            let text = encode(&request);
            assert_eq!(text, format!("{wire}\n"));
            assert_eq!(parse_request(&text).unwrap(), request);
            assert_eq!(serde_json::from_str::<Request>(wire).unwrap(), request);
        }
    }

    #[test]
    fn unit_calls_take_absent_empty_or_null_params() {
        for line in [
            r#"{"id":2,"method":"status"}"#,
            r#"{"id":2,"method":"status","params":{}}"#,
            r#"{"id":2,"method":"status","params":null}"#,
        ] {
            assert_eq!(parse_request(line).unwrap(), req(2, Call::Status), "{line}");
            assert_eq!(
                serde_json::from_str::<Request>(line).unwrap(),
                req(2, Call::Status)
            );
        }
    }

    #[test]
    fn pause_params_are_optional() {
        for line in [
            r#"{"id":3,"method":"pause_backend"}"#,
            r#"{"id":3,"method":"pause_backend","params":{}}"#,
        ] {
            assert_eq!(
                parse_request(line).unwrap(),
                req(3, Call::PauseBackend(PauseParams::default()))
            );
        }
    }

    #[test]
    fn an_unknown_method_is_unknown_method_with_the_id() {
        let (id, error) = parse_request(r#"{"id":9,"method":"format_disk"}"#).unwrap_err();
        assert_eq!(id, Some(9));
        assert_eq!(error.code, "unknown_method");
    }

    #[test]
    fn garbage_is_bad_request() {
        for line in [
            "",
            "nope",
            "[1,2]",
            r#"{"method":"status"}"#,
            r#"{"id":-1,"method":"status"}"#,
        ] {
            let (id, error) = parse_request(line).unwrap_err();
            assert_eq!(id, None, "{line}");
            assert_eq!(error.code, "bad_request", "{line}");
        }
        let (id, error) = parse_request(r#"{"id":4}"#).unwrap_err();
        assert_eq!((id, error.code.as_str()), (Some(4), "bad_request"));
        let (id, error) =
            parse_request(r#"{"id":4,"method":"pause_backend","params":{"drainTimeoutMs":"x"}}"#)
                .unwrap_err();
        assert_eq!((id, error.code.as_str()), (Some(4), "bad_request"));
    }

    #[test]
    fn events_serialize_as_documented() {
        let cases = [
            (
                Event::Hello {
                    protocol: 1,
                    bridge: "0.1.0".into(),
                    pid: 42,
                    instance: "00112233aabbccdd".into(),
                },
                r#"{"event":"hello","protocol":1,"bridge":"0.1.0","pid":42,"instance":"00112233aabbccdd","at":5}"#,
            ),
            (
                Event::StateChanged {
                    state: BridgeState::Waiting,
                    detail: Some("approval".into()),
                },
                r#"{"event":"state_changed","state":"waiting","detail":"approval","at":5}"#,
            ),
            (
                Event::StateChanged {
                    state: BridgeState::Reconnecting,
                    detail: None,
                },
                r#"{"event":"state_changed","state":"reconnecting","at":5}"#,
            ),
            (
                Event::TurnStarted {
                    source: "prompt".into(),
                },
                r#"{"event":"turn_started","source":"prompt","at":5}"#,
            ),
            (
                Event::TurnEnded { interrupted: true },
                r#"{"event":"turn_ended","interrupted":true,"at":5}"#,
            ),
            (
                Event::RequestOpened {
                    kind: "plan".into(),
                    open: 1,
                },
                r#"{"event":"request_opened","kind":"plan","open":1,"at":5}"#,
            ),
            (
                Event::RequestClosed {
                    kind: "plan".into(),
                    outcome: "expired".into(),
                    open: 0,
                },
                r#"{"event":"request_closed","kind":"plan","outcome":"expired","open":0,"at":5}"#,
            ),
            (
                Event::Error {
                    code: "backend_crashed".into(),
                    method: Some("restart_backend".into()),
                },
                r#"{"event":"error","code":"backend_crashed","method":"restart_backend","at":5}"#,
            ),
        ];
        for (event, wire) in cases {
            let line = EventLine { event, at: 5 };
            assert_eq!(encode(&line), format!("{wire}\n"));
            assert_eq!(
                serde_json::from_str::<ServerLine>(wire).unwrap(),
                ServerLine::Event(line)
            );
        }
    }

    #[test]
    fn every_state_has_its_snake_case_name() {
        let states = [
            (BridgeState::Starting, "starting"),
            (BridgeState::Idle, "idle"),
            (BridgeState::Working, "working"),
            (BridgeState::Waiting, "waiting"),
            (BridgeState::Reconnecting, "reconnecting"),
            (BridgeState::Restarting, "restarting"),
            (BridgeState::Paused, "paused"),
            (BridgeState::Stopping, "stopping"),
        ];
        for (state, name) in states {
            assert_eq!(serde_json::to_value(state).unwrap(), json!(name));
        }
    }

    #[test]
    fn server_lines_tell_responses_from_events() {
        let ok = r#"{"id":1,"ok":true,"result":{"x":1}}"#;
        assert_eq!(
            serde_json::from_str::<ServerLine>(ok).unwrap(),
            ServerLine::Response(Response {
                id: 1,
                ok: true,
                result: Some(json!({"x": 1})),
                error: None,
            })
        );
        let failed = Response {
            id: 2,
            ok: false,
            result: None,
            error: Some(ErrorBody {
                code: "busy".into(),
                message: "a pause is running".into(),
            }),
        };
        let wire = encode(&ServerLine::Response(failed.clone()));
        assert_eq!(
            wire,
            "{\"id\":2,\"ok\":false,\"error\":{\"code\":\"busy\",\"message\":\"a pause is running\"}}\n"
        );
        assert_eq!(
            serde_json::from_str::<ServerLine>(&wire).unwrap(),
            ServerLine::Response(failed)
        );
        let event = r#"{"event":"turn_ended","interrupted":false,"at":9}"#;
        assert!(matches!(
            serde_json::from_str::<ServerLine>(event).unwrap(),
            ServerLine::Event(_)
        ));
    }

    #[test]
    fn status_uses_camel_case_and_round_trips() {
        let status = Status {
            protocol: PROTOCOL,
            instance: "00112233aabbccdd".into(),
            pid: 1,
            bridge_version: "0.1.0".into(),
            state: Some(BridgeState::Paused),
            agent: Some(AgentInfo {
                display_name: "Coda".into(),
                handle: "coda".into(),
            }),
            backend: Some(BackendInfo {
                name: "coda".into(),
                version: None,
                running: false,
                running_tasks: 0,
            }),
            open_requests: 1,
            held_texts: 2,
            pause: Some(PauseInfo {
                phase: "waiting_for_file".into(),
                reason: None,
                since: 3,
            }),
            last_error: Some(LastError {
                code: "x".into(),
                at: 4,
            }),
            started_at: 5,
            ..Status::default()
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["bridgeVersion"], "0.1.0");
        assert_eq!(value["agent"]["displayName"], "Coda");
        assert_eq!(value["backend"]["runningTasks"], 0);
        assert_eq!(value["openRequests"], 1);
        assert_eq!(value["heldTexts"], 2);
        assert_eq!(value["pause"]["phase"], "waiting_for_file");
        assert_eq!(value["lastError"]["code"], "x");
        assert_eq!(value["startedAt"], 5);
        assert_eq!(serde_json::from_value::<Status>(value).unwrap(), status);
    }

    #[test]
    fn peek_params_are_optional() {
        for line in [
            r#"{"id":3,"method":"peek"}"#,
            r#"{"id":3,"method":"peek","params":null}"#,
            r#"{"id":3,"method":"peek","params":{}}"#,
        ] {
            assert_eq!(
                parse_request(line).unwrap(),
                req(3, Call::Peek(PeekParams::default())),
                "{line}"
            );
        }
    }

    #[test]
    fn bad_peek_params_are_bad_request_with_the_id() {
        for line in [
            r#"{"id":4,"method":"peek","params":{"limit":"x"}}"#,
            r#"{"id":4,"method":"peek","params":{"limit":-1}}"#,
            r#"{"id":4,"method":"peek","params":[1]}"#,
        ] {
            let (id, error) = parse_request(line).unwrap_err();
            assert_eq!(
                (id, error.code.as_str()),
                (Some(4), "bad_request"),
                "{line}"
            );
        }
    }

    #[test]
    fn peek_limit_defaults_and_clamps() {
        let limit = |limit| peek_limit(&PeekParams { limit });
        assert_eq!(limit(None), PEEK_DEFAULT);
        assert_eq!(limit(None), 20);
        assert_eq!(limit(Some(0)), 1);
        assert_eq!(limit(Some(7)), 7);
        assert_eq!(limit(Some(50)), 50);
        assert_eq!(limit(Some(500)), PEEK_CAP);
    }

    #[test]
    fn peek_entries_serialize_as_documented() {
        let card = PeekEntry {
            time: 5,
            from: PeekFrom::Agent,
            kind: PeekKind::Card,
            text: "Allow x? [Approve] [Reject]".into(),
            card_state: Some(CardState::Open),
        };
        assert_eq!(
            serde_json::to_string(&card).unwrap(),
            r#"{"time":5,"from":"agent","kind":"card","text":"Allow x? [Approve] [Reject]","card_state":"open"}"#
        );
        let text = PeekEntry {
            time: 6,
            from: PeekFrom::Owner,
            kind: PeekKind::Text,
            text: "hi".into(),
            card_state: None,
        };
        assert_eq!(
            serde_json::to_string(&text).unwrap(),
            r#"{"time":6,"from":"owner","kind":"text","text":"hi"}"#
        );
        let result = PeekResult {
            entries: vec![card, text],
        };
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["entries"].as_array().unwrap().len(), 2);
        assert_eq!(serde_json::from_value::<PeekResult>(value).unwrap(), result);
    }

    #[test]
    fn call_changed_and_the_call_state_serialize_as_documented() {
        let call = CallInfo {
            phase: CallPhase::InCall,
            direction: CallDirection::Outgoing,
            speaking: true,
            listening: false,
            muted: false,
            since: 4,
        };
        let line = EventLine {
            event: Event::CallChanged {
                call: Some(call.clone()),
            },
            at: 5,
        };
        let wire = r#"{"event":"call_changed","call":{"phase":"in_call","direction":"outgoing","speaking":true,"listening":false,"muted":false,"since":4},"at":5}"#;
        assert_eq!(encode(&line), format!("{wire}\n"));
        assert_eq!(
            serde_json::from_str::<ServerLine>(wire).unwrap(),
            ServerLine::Event(line)
        );
        let ended = EventLine {
            event: Event::CallChanged { call: None },
            at: 6,
        };
        assert_eq!(encode(&ended), "{\"event\":\"call_changed\",\"at\":6}\n");
        for (phase, name) in [
            (CallPhase::Ringing, "ringing"),
            (CallPhase::Connecting, "connecting"),
            (CallPhase::InCall, "in_call"),
        ] {
            assert_eq!(serde_json::to_value(phase).unwrap(), json!(name));
        }
        assert_eq!(
            serde_json::to_value(CallDirection::Incoming).unwrap(),
            json!("incoming")
        );
        let state = CallStateResult { call: Some(call) };
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["call"]["phase"], "in_call");
        assert_eq!(
            serde_json::to_value(CallStateResult::default()).unwrap(),
            json!({"call": null})
        );
        let status = Status::default();
        assert_eq!(serde_json::to_value(&status).unwrap()["call"], json!(null));
        // An older bridge's status has no call: it reads as none.
        let old: Status = serde_json::from_value(json!({"protocol": 1})).unwrap();
        assert_eq!(old.call, None);
    }

    #[test]
    fn the_adapter_status_and_adapter_changed_serialize_as_documented() {
        let ready = AdapterStatus {
            state: "ready".into(),
            name: Some("whisper".into()),
            version: Some("1.2.3".into()),
            capabilities: Some(vec!["stt".into(), "tts".into(), "barge_in".into()]),
            ..AdapterStatus::default()
        };
        let line = EventLine {
            event: Event::AdapterChanged {
                adapter: Some(ready.clone()),
            },
            at: 5,
        };
        let wire = r#"{"event":"adapter_changed","adapter":{"state":"ready","name":"whisper","version":"1.2.3","capabilities":["stt","tts","barge_in"]},"at":5}"#;
        assert_eq!(encode(&line), format!("{wire}\n"));
        assert_eq!(
            serde_json::from_str::<ServerLine>(wire).unwrap(),
            ServerLine::Event(line)
        );
        let backoff = AdapterStatus {
            state: "backoff".into(),
            reason: Some("not_found".into()),
            retry_at: Some(9),
            metrics: Some(AdapterMetrics {
                calls: 1,
                frames_to_adapter: 2,
                dropped_from_adapter: 3,
                restarts: 4,
                speak_to_audio: Some(LatencyInfo {
                    last_ms: 5,
                    avg_ms: 6,
                    max_ms: 7,
                }),
                ..AdapterMetrics::default()
            }),
            ..AdapterStatus::default()
        };
        let status = Status {
            adapter: Some(backoff),
            ..Status::default()
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["adapter"]["retryAt"], 9);
        assert_eq!(value["adapter"]["metrics"]["droppedFromAdapter"], 3);
        assert_eq!(value["adapter"]["metrics"]["speakToAudio"]["avgMs"], 6);
        assert_eq!(serde_json::from_value::<Status>(value).unwrap(), status);
        // Without an adapter the field is absent (older trays never see it) …
        let none = serde_json::to_value(Status::default()).unwrap();
        assert!(none.get("adapter").is_none(), "{none}");
        // … and an older bridge's status, or an adapter with fields this tray does not
        // know, still reads.
        let old: Status = serde_json::from_value(json!({"protocol": 1})).unwrap();
        assert_eq!(old.adapter, None);
        let newer: Status = serde_json::from_value(
            json!({"adapter": {"state": "starting", "future": 1, "metrics": {"x": 2}}}),
        )
        .unwrap();
        assert_eq!(newer.adapter.unwrap().state, "starting");
        let bare = EventLine {
            event: Event::AdapterChanged { adapter: None },
            at: 6,
        };
        assert_eq!(encode(&bare), "{\"event\":\"adapter_changed\",\"at\":6}\n");
    }

    #[test]
    fn mute_params_are_optional_and_checked() {
        for line in [
            r#"{"id":3,"method":"mute"}"#,
            r#"{"id":3,"method":"mute","params":null}"#,
            r#"{"id":3,"method":"mute","params":{}}"#,
        ] {
            assert_eq!(
                parse_request(line).unwrap(),
                req(3, Call::Mute(MuteParams::default())),
                "{line}"
            );
        }
        for line in [
            r#"{"id":4,"method":"mute","params":{"muted":"yes"}}"#,
            r#"{"id":4,"method":"mute","params":[true]}"#,
            r#"{"id":4,"method":"hangup","params":[1]}"#,
        ] {
            let (id, error) = parse_request(line).unwrap_err();
            assert_eq!(
                (id, error.code.as_str()),
                (Some(4), "bad_request"),
                "{line}"
            );
        }
    }

    #[test]
    fn peek_changed_is_a_bare_event() {
        let line = EventLine {
            event: Event::PeekChanged,
            at: 5,
        };
        let wire = r#"{"event":"peek_changed","at":5}"#;
        assert_eq!(encode(&line), format!("{wire}\n"));
        assert_eq!(
            serde_json::from_str::<ServerLine>(wire).unwrap(),
            ServerLine::Event(line)
        );
    }

    /// Only `peek` may carry message text. Both matches are exhaustive on purpose: a new
    /// call or event does not compile until someone decides whether it carries content.
    #[test]
    fn status_fields_are_content_free() {
        // Destructured without `..`: a new field does not compile until it is classified.
        let Status {
            protocol: _,       // a number
            instance: _,       // the instance id (a hash of the folder)
            pid: _,            // a number
            bridge_version: _, // a version
            state: _,          // a state
            detail: _,         // a short fixed label ("approval", "offline", …)
            agent: _,          // the agent's own display name and handle
            connection: _,     // a connection state
            folder: _,         // the backend's working folder
            session: _,        // a session id
            backend: _,        // see BackendInfo
            open_requests: _,  // a count
            held_texts: _,     // a count, never the texts
            pause: _,          // see PauseInfo
            last_error: _,     // see LastError
            started_at: _,     // a time
            call: _,           // see CallInfo
            adapter: _,        // see AdapterStatus
        } = Status::default();
        let AdapterStatus {
            state: _,        // a fixed state name
            name: _,         // the adapter's program name from its hello
            version: _,      // its version
            capabilities: _, // fixed capability names
            reason: _,       // a fixed phrase or an error code, never a message's text
            retry_at: _,     // a time
            metrics: _,      // see AdapterMetrics
        } = AdapterStatus::default();
        let AdapterMetrics {
            calls: _,                // counts …
            frames_to_adapter: _,    //
            frames_from_adapter: _,  //
            dropped_to_adapter: _,   //
            dropped_from_adapter: _, //
            restarts: _,             //
            speaks: _,               //
            final_to_speak: _,       // durations
            speak_to_audio: _,       //
        } = AdapterMetrics::default();
        let LatencyInfo {
            last_ms: _,
            avg_ms: _,
            max_ms: _,
        } = LatencyInfo::default();
        let CallInfo {
            phase: _,     // a fixed phase name
            direction: _, // incoming or outgoing
            speaking: _,  // a flag
            listening: _, // a flag
            muted: _,     // a flag
            since: _,     // a time
        } = CallInfo {
            phase: CallPhase::Ringing,
            direction: CallDirection::Incoming,
            speaking: false,
            listening: false,
            muted: false,
            since: 0,
        };
        let AgentInfo {
            display_name: _,
            handle: _,
        } = AgentInfo {
            display_name: String::new(),
            handle: String::new(),
        };
        let BackendInfo {
            name: _,          // the backend's name
            version: _,       // its version
            running: _,       // a flag
            running_tasks: _, // a count
        } = BackendInfo {
            name: String::new(),
            version: None,
            running: false,
            running_tasks: 0,
        };
        let PauseInfo {
            phase: _,  // a fixed phase name
            reason: _, // the caller's reason (the tray's or the CLI's), never a chat's
            since: _,  // a time
        } = PauseInfo {
            phase: String::new(),
            reason: None,
            since: 0,
        };
        let LastError {
            code: _, // an error code
            at: _,   // a time
        } = LastError {
            code: String::new(),
            at: 0,
        };
    }

    #[test]
    fn only_peek_carries_message_text() {
        fn carries_text(call: &Call) -> bool {
            match call {
                // No parameters; the result is the content-free `Status`.
                Call::Status => false,
                // No parameters; the result is ok or an error code.
                Call::RestartBackend | Call::Reconnect | Call::Shutdown => false,
                Call::ResumeBackend => false,
                // A reason for people from the caller, and a timeout; nothing from chats.
                Call::PauseBackend(_) => false,
                // The owner conversation's last messages, masked and capped, when allowed.
                Call::Peek(_) => true,
                // No parameters; the result is a content-free `CallInfo`.
                Call::CallState | Call::Hangup => false,
                // A flag.
                Call::Mute(_) => false,
            }
        }
        fn event_carries_text(event: &Event) -> bool {
            match event {
                // Versions, a pid and the instance id.
                Event::Hello { .. } => false,
                // A state and a short fixed detail.
                Event::StateChanged { .. } => false,
                // A fixed source name.
                Event::TurnStarted { .. } | Event::TurnEnded { .. } => false,
                // Kinds, outcomes and counts.
                Event::RequestOpened { .. } | Event::RequestClosed { .. } => false,
                // An error code and a method name.
                Event::Error { .. } => false,
                // Only that the peek changed; clients ask `peek` for the entries.
                Event::PeekChanged => false,
                // A phase, a direction, flags and a time.
                Event::CallChanged { .. } => false,
                // A state, the adapter's name, version and capabilities, a fixed reason.
                Event::AdapterChanged { .. } => false,
            }
        }
        let calls = [
            Call::Status,
            Call::RestartBackend,
            Call::Reconnect,
            Call::Shutdown,
            Call::PauseBackend(PauseParams::default()),
            Call::ResumeBackend,
            Call::Peek(PeekParams::default()),
            Call::CallState,
            Call::Mute(MuteParams::default()),
            Call::Hangup,
        ];
        for call in &calls {
            assert_eq!(
                carries_text(call),
                matches!(call, Call::Peek(_)),
                "{}",
                call.method()
            );
        }
        let events = [
            Event::Hello {
                protocol: PROTOCOL,
                bridge: "0.1.0".into(),
                pid: 1,
                instance: "x".into(),
            },
            Event::StateChanged {
                state: BridgeState::Idle,
                detail: None,
            },
            Event::TurnStarted {
                source: "prompt".into(),
            },
            Event::TurnEnded { interrupted: false },
            Event::RequestOpened {
                kind: "plan".into(),
                open: 1,
            },
            Event::RequestClosed {
                kind: "plan".into(),
                outcome: "answered".into(),
                open: 0,
            },
            Event::Error {
                code: "x".into(),
                method: None,
            },
            Event::PeekChanged,
            Event::CallChanged { call: None },
            Event::AdapterChanged { adapter: None },
        ];
        for event in &events {
            assert!(!event_carries_text(event), "{event:?}");
        }
    }

    #[test]
    fn now_is_after_2020() {
        assert!(now_ms() > 1_577_836_800_000);
    }
}
