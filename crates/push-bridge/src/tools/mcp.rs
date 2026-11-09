//! `push-agent-bridge tools-mcp`: a small stdio MCP server (newline-delimited JSON-RPC
//! 2.0) that Coda starts from the `.mcp.json` the Coda backend writes. It offers the
//! bridge's actions as tools and forwards each call over the tools endpoint
//! ([`super::ENDPOINT_ENV`]) with the run's token ([`super::TOKEN_ENV`]); the result is
//! the tool's text (the [`ActionResult`] as JSON), an error for every code but `done` and
//! `waiting_for_owner`.

use std::io;
use std::sync::Arc;

use push_bridge_control::Endpoint;
use push_bridge_control::MAX_LINE;
use push_bridge_control::transport::{Stream, connect_same_user};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::{ENDPOINT_ENV, Line, Refusal, Reply, TOKEN_ENV, next_line, parse_endpoint};
use crate::actions::{Action, ActionCode, ActionResult};

/// The MCP versions this server speaks; it answers a client's version it knows with that
/// version, any other with the latest.
pub const PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
/// The latest of [`PROTOCOL_VERSIONS`].
pub const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
/// The server's name: Coda shows the tools as `mcp__push__<tool>`.
pub const SERVER_NAME: &str = "push";

/// The longest line read from the client.
const MAX_INPUT: usize = 4 << 20;
const PARSE_ERROR: i64 = -32700;
const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;

/// What the model is told about the tools' results.
const INSTRUCTIONS: &str = "\
These tools act on push-messenger for your owner: messages, contact requests, groups and \
calls. Each returns JSON {code, detail, ...}. code \"done\": carried out. \
\"waiting_for_owner\": your owner decides; the bridge finishes it once approved (do not \
retry; \"outgoing\" lists what waits). Errors: \"not_allowed_by_owner\" (your owner turned \
it off), \"rate_limited\" (retry after retry_after_s seconds), \"paused\" (the chat is \
paused until a person writes), \"not_found\", \"not_contact\" (send request_contact \
first), \"not_allowed\", \"unavailable\", \"failed\".";

/// How a tool call reaches the bridge.
#[async_trait::async_trait]
pub trait Connector: Send + Sync {
    async fn connect(&self) -> io::Result<Stream>;
}

/// The tools endpoint, over the same-user transport.
pub struct EndpointConnector(pub Endpoint);

#[async_trait::async_trait]
impl Connector for EndpointConnector {
    async fn connect(&self) -> io::Result<Stream> {
        connect_same_user(&self.0).await
    }
}

/// No endpoint was given: every call fails, saying so.
struct NoEndpoint;

#[async_trait::async_trait]
impl Connector for NoEndpoint {
    async fn connect(&self) -> io::Result<Stream> {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{ENDPOINT_ENV} is not set"),
        ))
    }
}

/// Serves stdin and stdout with the endpoint and token from the environment.
pub async fn run_stdio() -> io::Result<()> {
    let connector: Arc<dyn Connector> = match std::env::var(ENDPOINT_ENV)
        .ok()
        .as_deref()
        .and_then(parse_endpoint)
    {
        Some(endpoint) => Arc::new(EndpointConnector(endpoint)),
        None => Arc::new(NoEndpoint),
    };
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
    serve(tokio::io::stdin(), tokio::io::stdout(), connector, &token).await
}

/// Serves one MCP client on `input` and `output` until `input` ends; tool calls run
/// concurrently, their answers in the order they finish.
pub async fn serve<R, W>(
    input: R,
    output: W,
    connector: Arc<dyn Connector>,
    token: &str,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let token: Arc<str> = Arc::from(token);
    let (lines, mut outgoing) = mpsc::unbounded_channel::<Value>();
    let writing = tokio::spawn(async move {
        let mut output = output;
        while let Some(message) = outgoing.recv().await {
            let mut line = message.to_string();
            line.push('\n');
            if output.write_all(line.as_bytes()).await.is_err() || output.flush().await.is_err() {
                return;
            }
        }
    });
    let mut reader = BufReader::new(input);
    let mut calls = JoinSet::new();
    let read = loop {
        let text = match next_line(&mut reader, MAX_INPUT).await {
            Ok(Some(Line::Text(text))) => text,
            Ok(Some(Line::TooLong)) => {
                let _ = lines.send(error(Value::Null, PARSE_ERROR, "the line is too long"));
                break Ok(());
            }
            Ok(None) => break Ok(()),
            Err(failure) => break Err(failure),
        };
        let Ok(message) = serde_json::from_str::<Value>(&text) else {
            let _ = lines.send(error(Value::Null, PARSE_ERROR, "not JSON"));
            continue;
        };
        // Notifications (no id) and the client's responses (no method) need no answer.
        let (Some(method), Some(id)) = (
            message.get("method").and_then(Value::as_str),
            message.get("id").cloned(),
        ) else {
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                let _ = lines.send(result(id, initialize(&params)));
            }
            "ping" => {
                let _ = lines.send(result(id, json!({})));
            }
            "tools/list" => {
                let _ = lines.send(result(id, json!({ "tools": tools() })));
            }
            "tools/call" => match tool_action(&params) {
                Err(message) => {
                    let _ = lines.send(error(id, INVALID_PARAMS, &message));
                }
                Ok(Err(message)) => {
                    let _ = lines.send(result(id, tool_text(&message, true)));
                }
                Ok(Ok(action)) => {
                    let (connector, token, lines) =
                        (Arc::clone(&connector), Arc::clone(&token), lines.clone());
                    calls.spawn(async move {
                        let answer = forward(connector.as_ref(), &token, &action).await;
                        let _ = lines.send(result(id, answer));
                    });
                }
            },
            other => {
                let _ = lines.send(error(
                    id,
                    METHOD_NOT_FOUND,
                    &format!("method not found: {other}"),
                ));
            }
        }
    };
    while calls.join_next().await.is_some() {}
    drop(lines);
    let _ = writing.await;
    read
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|asked| PROTOCOL_VERSIONS.contains(asked))
        .unwrap_or(LATEST_PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

/// A JSON Schema string property.
fn text_property(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// The tools, as `tools/list` gives them.
pub fn tools() -> Vec<Value> {
    let handle = "A push-messenger handle, like \"@alice\".";
    vec![
        json!({
            "name": "request_contact",
            "description": "Ask a push-messenger user to become your contact. You need an \
                accepted contact before you can message, group or call them.",
            "inputSchema": object_schema(json!({ "handle": text_property(handle) }), &["handle"]),
        }),
        json!({
            "name": "send_message",
            "description": "Send a text message: to your owner (\"owner\"), to a contact \
                (\"@handle\"; opens a direct chat with them if needed) or into a group you \
                are in (\"group:<id>\"). Use it to answer a message someone sent you.",
            "inputSchema": object_schema(
                json!({
                    "to": text_property("\"owner\", \"@handle\" or \"group:<id>\"."),
                    "text": text_property("The message."),
                }),
                &["to", "text"],
            ),
        }),
        json!({
            "name": "notify_owner",
            "description": "Post a short note to your owner in their chat with you, for \
                news they should see even though they did not ask.",
            "inputSchema": object_schema(
                json!({ "text": text_property("The note.") }),
                &["text"],
            ),
        }),
        json!({
            "name": "create_group",
            "description": "Create a group chat with accepted contacts. The result's \
                \"conversation\" is the group (\"group:<id>\") for send_message and invite.",
            "inputSchema": object_schema(
                json!({
                    "name": text_property("The group's name."),
                    "members": {
                        "type": "array",
                        "items": text_property(handle),
                        "minItems": 1,
                        "description": "The accepted contacts to add, by handle.",
                    },
                }),
                &["name", "members"],
            ),
        }),
        json!({
            "name": "invite",
            "description": "Add an accepted contact to a group you are in.",
            "inputSchema": object_schema(
                json!({
                    "group": text_property("The group, \"group:<id>\"."),
                    "member": text_property(handle),
                }),
                &["group", "member"],
            ),
        }),
        json!({
            "name": "call",
            "description": "Place a voice call to your owner (\"owner\") or an accepted \
                contact (\"@handle\"); the reason is said when they pick up.",
            "inputSchema": object_schema(
                json!({
                    "to": text_property("\"owner\" or \"@handle\"."),
                    "reason": text_property("Why you call, said when they pick up."),
                }),
                &["to"],
            ),
        }),
        json!({
            "name": "outgoing",
            "description": "List your actions waiting for your owner's approval and those \
                resolved lately.",
            "inputSchema": object_schema(json!({}), &[]),
            "annotations": { "readOnlyHint": true },
        }),
    ]
}

/// The action a `tools/call` asks for: `Err` for a protocol error (no such tool),
/// `Ok(Err)` for arguments that do not fit (a tool error the model can correct).
fn tool_action(params: &Value) -> Result<Result<Action, String>, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tools/call needs a tool name")?;
    if !tools().iter().any(|tool| tool["name"] == name) {
        return Err(format!("unknown tool: {name}"));
    }
    let mut arguments = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(arguments)) => arguments.clone(),
        Some(_) => return Err("the arguments must be an object".to_owned()),
    };
    arguments.insert("action".to_owned(), json!(name));
    Ok(serde_json::from_value(Value::Object(arguments))
        .map_err(|failure| format!("invalid arguments for {name}: {failure}")))
}

fn tool_text(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// Runs `action` on the bridge; the tool's result.
async fn forward(connector: &dyn Connector, token: &str, action: &Action) -> Value {
    match ask(connector, token, action).await {
        Ok(Reply::Result(result)) => tool_result(&result),
        Ok(Reply::Error(Refusal::Unauthorized)) => tool_text(
            "The push bridge refused this tool's token (unauthorized): Coda was started by \
             another run of the bridge.",
            true,
        ),
        Ok(Reply::Error(Refusal::Invalid)) => tool_text(
            "The push bridge did not accept the request (invalid).",
            true,
        ),
        Err(failure) => tool_text(
            &format!(
                "The push bridge cannot be reached ({failure}); these tools work only while \
                 the bridge runs."
            ),
            true,
        ),
    }
}

fn tool_result(result: &ActionResult) -> Value {
    let text = serde_json::to_string(result).expect("a result serializes");
    let ok = matches!(result.code, ActionCode::Done | ActionCode::WaitingForOwner);
    tool_text(&text, !ok)
}

async fn ask(connector: &dyn Connector, token: &str, action: &Action) -> io::Result<Reply> {
    let stream = connector.connect().await?;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut line = json!({ "token": token, "action": action }).to_string();
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    let mut reader = BufReader::new(reader);
    match next_line(&mut reader, MAX_LINE).await? {
        Some(Line::Text(text)) => serde_json::from_str(&text)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "an unreadable answer")),
        _ => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the connection closed without an answer",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, DuplexStream, Lines, ReadHalf, WriteHalf};

    use super::*;
    use crate::actions::Target;

    const WAIT: Duration = Duration::from_secs(5);

    /// A fake endpoint: records each request line and answers with the next scripted
    /// reply; with none left, refuses connections.
    #[derive(Default)]
    struct FakeEndpoint {
        replies: Mutex<Vec<Value>>,
        seen: Arc<Mutex<Vec<Value>>>,
    }

    #[async_trait::async_trait]
    impl Connector for FakeEndpoint {
        async fn connect(&self) -> io::Result<Stream> {
            let Some(reply) = self.replies.lock().unwrap().pop() else {
                return Err(io::Error::new(io::ErrorKind::NotFound, "nobody listens"));
            };
            let (client, server) = tokio::io::duplex(1 << 16);
            let seen = Arc::clone(&self.seen);
            tokio::spawn(async move {
                let (reader, mut writer) = tokio::io::split(server);
                let mut lines = BufReader::new(reader).lines();
                if let Ok(Some(line)) = lines.next_line().await {
                    seen.lock()
                        .unwrap()
                        .push(serde_json::from_str(&line).unwrap());
                    let mut line = reply.to_string();
                    line.push('\n');
                    let _ = writer.write_all(line.as_bytes()).await;
                }
            });
            Ok(Box::new(client))
        }
    }

    struct Client {
        lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
        writer: WriteHalf<DuplexStream>,
        next_id: u64,
    }

    impl Client {
        fn start(endpoint: Arc<FakeEndpoint>) -> Self {
            let (client, server) = tokio::io::duplex(1 << 16);
            let (input, output) = tokio::io::split(server);
            tokio::spawn(async move { serve(input, output, endpoint, "tok-1").await });
            let (reader, writer) = tokio::io::split(client);
            Self {
                lines: BufReader::new(reader).lines(),
                writer,
                next_id: 0,
            }
        }

        async fn send(&mut self, message: Value) {
            let mut line = message.to_string();
            line.push('\n');
            self.writer.write_all(line.as_bytes()).await.unwrap();
        }

        async fn next(&mut self) -> Value {
            let line = tokio::time::timeout(WAIT, self.lines.next_line())
                .await
                .expect("timed out")
                .unwrap()
                .expect("a line");
            serde_json::from_str(&line).unwrap()
        }

        /// Sends a request; its response.
        async fn request(&mut self, method: &str, params: Value) -> Value {
            self.next_id += 1;
            let id = self.next_id;
            self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .await;
            let response = self.next().await;
            assert_eq!(response["id"], id, "{response}");
            response
        }

        async fn call(&mut self, name: &str, arguments: Value) -> Value {
            self.request("tools/call", json!({"name": name, "arguments": arguments}))
                .await
        }
    }

    fn endpoint(replies: &[Value]) -> Arc<FakeEndpoint> {
        let endpoint = FakeEndpoint::default();
        *endpoint.replies.lock().unwrap() = replies.iter().rev().cloned().collect();
        Arc::new(endpoint)
    }

    #[tokio::test]
    async fn initialize_offers_tools_and_names_the_server() {
        let mut client = Client::start(endpoint(&[]));
        let response = client
            .request(
                "initialize",
                json!({"protocolVersion": "2025-03-26", "capabilities": {},
                       "clientInfo": {"name": "coda", "version": "1"}}),
            )
            .await;
        let result = &response["result"];
        assert_eq!(result["protocolVersion"], "2025-03-26");
        assert_eq!(result["capabilities"], json!({"tools": {}}));
        assert_eq!(result["serverInfo"]["name"], "push");
        assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        client
            .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        let response = client
            .request("initialize", json!({"protocolVersion": "1999-01-01"}))
            .await;
        assert_eq!(
            response["result"]["protocolVersion"],
            LATEST_PROTOCOL_VERSION
        );
        assert_eq!(client.request("ping", json!({})).await["result"], json!({}));
    }

    #[tokio::test]
    async fn tools_list_has_the_seven_actions_with_schemas() {
        let mut client = Client::start(endpoint(&[]));
        let response = client.request("tools/list", Value::Null).await;
        let tools = response["result"]["tools"].as_array().unwrap().clone();
        let names: Vec<_> = tools.iter().map(|tool| tool["name"].clone()).collect();
        assert_eq!(
            names,
            [
                "request_contact",
                "send_message",
                "notify_owner",
                "create_group",
                "invite",
                "call",
                "outgoing"
            ]
        );
        for tool in &tools {
            assert!(!tool["description"].as_str().unwrap().is_empty());
            assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        }
        assert_eq!(tools[1]["inputSchema"]["required"], json!(["to", "text"]));
        assert_eq!(
            tools[3]["inputSchema"]["properties"]["members"]["type"],
            "array"
        );
        assert_eq!(tools[5]["inputSchema"]["required"], json!(["to"]));
    }

    #[tokio::test]
    async fn a_call_forwards_the_action_with_the_token_and_returns_the_result() {
        let fake = endpoint(&[json!({"result": {"code": "done", "detail": "sent",
                                                "conversation": "c-1"}})]);
        let mut client = Client::start(Arc::clone(&fake));
        let response = client
            .call("send_message", json!({"to": "@bob", "text": "hi there"}))
            .await;
        let seen = fake.seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            [json!({"token": "tok-1",
                    "action": {"action": "send_message", "to": "@bob", "text": "hi there"}})]
        );
        let result = &response["result"];
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["type"], "text");
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            text,
            json!({"code": "done", "detail": "sent", "conversation": "c-1"})
        );
        let parsed: Action = serde_json::from_value(seen[0]["action"].clone()).unwrap();
        assert_eq!(
            parsed,
            Action::SendMessage {
                to: Target::Handle("bob".into()),
                text: "hi there".into()
            }
        );
    }

    #[tokio::test]
    async fn only_done_and_waiting_for_owner_are_no_errors() {
        let fake = endpoint(&[
            json!({"result": {"code": "waiting_for_owner", "detail": "w", "action_id": "a-1"}}),
            json!({"result": {"code": "not_allowed_by_owner", "detail": "n"}}),
            json!({"result": {"code": "paused", "detail": "p"}}),
            json!({"error": "unauthorized"}),
        ]);
        let mut client = Client::start(fake);
        let waiting = client.call("call", json!({"to": "owner"})).await;
        assert_eq!(waiting["result"]["isError"], false);
        assert!(
            waiting["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("\"action_id\":\"a-1\"")
        );
        for _ in 0..2 {
            let refused = client.call("outgoing", json!({})).await;
            assert_eq!(refused["result"]["isError"], true, "{refused}");
        }
        let unauthorized = client.call("notify_owner", json!({"text": "x"})).await;
        assert_eq!(unauthorized["result"]["isError"], true);
        assert!(
            unauthorized["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("unauthorized")
        );
    }

    #[tokio::test]
    async fn a_call_reaches_the_bridge_over_the_real_endpoint() {
        use crate::backend::BackendEvent;
        use crate::tools::endpoint::{ActionSink, ToolsServer};
        let id = format!("tm-{}", &crate::tools::new_token()[..12]);
        #[cfg(unix)]
        let root = crate::test_dir::TestDir::new();
        #[cfg(unix)]
        let endpoint = push_bridge_control::endpoint_for(&id, Some(root.path().as_os_str()));
        #[cfg(windows)]
        let endpoint = push_bridge_control::endpoint_for(&id, None);
        let endpoint = endpoint.unwrap();
        let listener = push_bridge_control::transport::Listener::bind(&endpoint).unwrap();
        let sink = ActionSink::new();
        let (events, mut actions) = mpsc::unbounded_channel();
        sink.attach(events);
        let serving = ToolsServer::new("tok-1", sink).serve(listener);
        let core = tokio::spawn(async move {
            let Some(BackendEvent::Action(request)) = actions.recv().await else {
                panic!("no action");
            };
            let _ = request.reply.send(ActionResult::done("noted"));
            request.action
        });

        let (client, server) = tokio::io::duplex(1 << 16);
        let (input, output) = tokio::io::split(server);
        let connector = Arc::new(EndpointConnector(endpoint));
        tokio::spawn(async move { serve(input, output, connector, "tok-1").await });
        let (reader, writer) = tokio::io::split(client);
        let mut client = Client {
            lines: BufReader::new(reader).lines(),
            writer,
            next_id: 0,
        };
        let response = client
            .call("notify_owner", json!({"text": "build green"}))
            .await;
        assert_eq!(response["result"]["isError"], false, "{response}");
        assert_eq!(
            tokio::time::timeout(WAIT, core).await.unwrap().unwrap(),
            Action::NotifyOwner {
                text: "build green".into()
            }
        );
        serving.abort();
    }

    #[tokio::test]
    async fn an_unreachable_bridge_is_a_tool_error_that_says_so() {
        let mut client = Client::start(endpoint(&[]));
        let response = client
            .call("request_contact", json!({"handle": "@al"}))
            .await;
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("cannot be reached"), "{text}");
    }

    #[tokio::test]
    async fn unknown_tools_bad_arguments_and_methods_are_errors() {
        let fake = endpoint(&[]);
        let mut client = Client::start(Arc::clone(&fake));
        let unknown = client.call("fly", json!({})).await;
        assert_eq!(unknown["error"]["code"], INVALID_PARAMS);
        let missing = client.call("send_message", json!({"to": "@bob"})).await;
        assert_eq!(missing["result"]["isError"], true);
        assert!(
            missing["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("invalid arguments")
        );
        let method = client.request("resources/list", json!({})).await;
        assert_eq!(method["error"]["code"], METHOD_NOT_FOUND);
        client.send(json!("not a request")).await;
        client.writer.write_all(b"not json\n").await.unwrap();
        assert_eq!(client.next().await["error"]["code"], PARSE_ERROR);
        assert!(fake.seen.lock().unwrap().is_empty(), "nothing forwarded");
    }
}
