//! The wire format: newline-delimited JSON, one UTF-8 object per line, at most
//! [`MAX_LINE`] bytes. Clients send [`Request`]s; the bridge sends [`ServerLine`]s, a
//! [`Response`] per request and [`EventLine`]s as things happen. Everything here is
//! content-free: states, counts, names and error codes, never message text.

use serde::de::Error as _;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// The protocol version in `hello` and `status`.
pub const PROTOCOL: u32 = 1;
/// The longest line either side accepts, in bytes.
pub const MAX_LINE: usize = 64 * 1024;

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
        if let Call::PauseBackend(params) = &self.call {
            map.serialize_entry("params", params)?;
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
        }
    }
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
    fn now_is_after_2020() {
        assert!(now_ms() > 1_577_836_800_000);
    }
}
