//! The bridge's side of the tools endpoint (see [`super`]): checks each line's token and
//! runs its action as the backend's own ([`BackendEvent::Action`]), answering with the
//! result.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use push_bridge_control::MAX_LINE;
use push_bridge_control::transport::Listener;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;

use super::{Line, Refusal, Reply, next_line};
use crate::actions::{Action, ActionCode, ActionRequest, ActionResult};
use crate::backend::BackendEvent;

/// Connections served at once; more are closed at once.
const MAX_CONNECTIONS: usize = 16;

/// Where the endpoint's actions go: the running bridge's backend events. A bridge run
/// attaches its channel; between runs actions answer `unavailable`.
#[derive(Clone, Default)]
pub struct ActionSink(Arc<Mutex<Option<mpsc::UnboundedSender<BackendEvent>>>>);

impl ActionSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// From now on actions go to `events` (the backend events of the bridge that runs).
    pub fn attach(&self, events: mpsc::UnboundedSender<BackendEvent>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(events);
    }

    /// Runs `action` as the backend's own; resolves with its result.
    pub async fn run(&self, action: Action) -> ActionResult {
        let (request, result) = ActionRequest::new(action);
        let sent = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|events| events.send(BackendEvent::Action(request)).is_ok());
        if !sent {
            return not_running();
        }
        result.await.unwrap_or_else(|_| not_running())
    }
}

fn not_running() -> ActionResult {
    ActionResult::new(ActionCode::Unavailable, "the bridge is not connected")
}

/// Serves the tools endpoint with the run's `token`.
#[derive(Clone)]
pub struct ToolsServer {
    token: Arc<str>,
    sink: ActionSink,
    slots: Arc<Semaphore>,
}

impl ToolsServer {
    pub fn new(token: &str, sink: ActionSink) -> Self {
        Self {
            token: Arc::from(token),
            sink,
            slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        }
    }

    /// Serves `listener`'s clients until the task is aborted.
    pub fn serve(&self, mut listener: Listener) -> JoinHandle<()> {
        let server = self.clone();
        tokio::spawn(async move {
            let mut refused = 0;
            loop {
                match listener.accept().await {
                    Ok(stream) => {
                        server.serve_stream(stream);
                    }
                    Err(error) => {
                        tracing::warn!(event = "tools_accept_failed", kind = ?error.kind());
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
                if listener.refused() != refused {
                    refused = listener.refused();
                    tracing::warn!(event = "tools_peer_refused", total = refused);
                }
            }
        })
    }

    /// Serves one connected stream until the client goes, sends a wrong token or a line
    /// over [`MAX_LINE`].
    pub fn serve_stream<S>(&self, stream: S) -> JoinHandle<()>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let server = self.clone();
        tokio::spawn(async move {
            let Ok(_slot) = Arc::clone(&server.slots).try_acquire_owned() else {
                tracing::warn!(event = "tools_connection_refused");
                return;
            };
            server.connection(stream).await;
        })
    }

    async fn connection<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin,
    {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);
        loop {
            let (reply, close) = match next_line(&mut reader, MAX_LINE).await {
                Ok(Some(Line::Text(text))) => match self.check(&text) {
                    Ok(action) => {
                        tracing::debug!(event = "tools_action", kind = action.kind());
                        (Reply::Result(self.sink.run(action).await), false)
                    }
                    Err(Refusal::Unauthorized) => {
                        tracing::warn!(event = "tools_refused");
                        (Reply::Error(Refusal::Unauthorized), true)
                    }
                    Err(Refusal::Invalid) => (Reply::Error(Refusal::Invalid), false),
                },
                Ok(Some(Line::TooLong)) => (Reply::Error(Refusal::Invalid), true),
                Ok(None) | Err(_) => return,
            };
            let mut line = serde_json::to_string(&reply).expect("a reply serializes");
            line.push('\n');
            let written =
                writer.write_all(line.as_bytes()).await.is_ok() && writer.flush().await.is_ok();
            if close || !written {
                let _ = writer.shutdown().await;
                return;
            }
        }
    }

    /// The line's action, once its token is the run's.
    fn check(&self, text: &str) -> Result<Action, Refusal> {
        let Ok(Value::Object(mut request)) = serde_json::from_str::<Value>(text) else {
            return Err(Refusal::Invalid);
        };
        let token = match request.get("token") {
            Some(Value::String(token)) => token.as_bytes(),
            _ => return Err(Refusal::Unauthorized),
        };
        if !same(token, self.token.as_bytes()) {
            return Err(Refusal::Unauthorized);
        }
        let action = request.remove("action").ok_or(Refusal::Invalid)?;
        serde_json::from_value(action).map_err(|_| Refusal::Invalid)
    }
}

/// Compares in time independent of where the bytes differ.
fn same(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, DuplexStream};

    use super::*;
    use crate::actions::Target;

    const TOKEN: &str = "abc123";
    const WAIT: Duration = Duration::from_secs(5);

    struct Client {
        lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<DuplexStream>>>,
        writer: tokio::io::WriteHalf<DuplexStream>,
    }

    impl Client {
        fn connect(server: &ToolsServer) -> Self {
            let (client, stream) = tokio::io::duplex(MAX_LINE * 4);
            server.serve_stream(stream);
            let (reader, writer) = tokio::io::split(client);
            Self {
                lines: BufReader::new(reader).lines(),
                writer,
            }
        }

        async fn send(&mut self, line: &str) {
            self.writer
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
        }

        async fn next(&mut self) -> Option<Value> {
            let line = tokio::time::timeout(WAIT, self.lines.next_line())
                .await
                .expect("timed out")
                .ok()??;
            Some(serde_json::from_str(&line).unwrap())
        }
    }

    /// A server whose actions reach the returned receiver as backend events.
    fn server() -> (ToolsServer, mpsc::UnboundedReceiver<BackendEvent>) {
        let sink = ActionSink::new();
        let (events, receiver) = mpsc::unbounded_channel();
        sink.attach(events);
        (ToolsServer::new(TOKEN, sink), receiver)
    }

    /// Answers the next action with `result`; returns the action.
    async fn answer(
        events: &mut mpsc::UnboundedReceiver<BackendEvent>,
        result: ActionResult,
    ) -> Action {
        let event = tokio::time::timeout(WAIT, events.recv())
            .await
            .expect("timed out")
            .expect("an event");
        let BackendEvent::Action(request) = event else {
            panic!("not an action: {event:?}");
        };
        let _ = request.reply.send(result);
        request.action
    }

    #[tokio::test]
    async fn the_right_token_runs_actions_one_per_line() {
        let (server, mut events) = server();
        let mut client = Client::connect(&server);
        client
            .send(
                r#"{"token":"abc123","action":{"action":"send_message","to":"@bob","text":"hi"}}"#,
            )
            .await;
        let action = answer(&mut events, ActionResult::done("sent")).await;
        assert_eq!(
            action,
            Action::SendMessage {
                to: Target::Handle("bob".into()),
                text: "hi".into()
            }
        );
        assert_eq!(
            client.next().await,
            Some(json!({"result": {"code": "done", "detail": "sent"}}))
        );
        client
            .send(r#"{"token":"abc123","action":{"action":"outgoing"}}"#)
            .await;
        assert_eq!(
            answer(&mut events, ActionResult::done("none")).await,
            Action::Outgoing
        );
        assert_eq!(client.next().await.unwrap()["result"]["detail"], "none");
    }

    #[tokio::test]
    async fn a_wrong_token_is_refused_and_the_connection_closed() {
        let (server, mut events) = server();
        for line in [
            r#"{"token":"abc124","action":{"action":"outgoing"}}"#,
            r#"{"token":"abc1234","action":{"action":"outgoing"}}"#,
            r#"{"action":{"action":"outgoing"}}"#,
        ] {
            let mut client = Client::connect(&server);
            client.send(line).await;
            assert_eq!(client.next().await, Some(json!({"error": "unauthorized"})));
            assert_eq!(client.next().await, None, "closed");
        }
        assert!(events.try_recv().is_err(), "no action ran");
    }

    #[tokio::test]
    async fn invalid_lines_are_answered_and_the_connection_stays() {
        let (server, mut events) = server();
        let mut client = Client::connect(&server);
        for line in [
            "not json",
            "[1]",
            r#"{"token":"abc123"}"#,
            r#"{"token":"abc123","action":{"action":"fly"}}"#,
            r#"{"token":"abc123","action":{"action":"send_message","to":"","text":"x"}}"#,
        ] {
            client.send(line).await;
            assert_eq!(
                client.next().await,
                Some(json!({"error": "invalid"})),
                "{line}"
            );
        }
        client
            .send(r#"{"token":"abc123","action":{"action":"notify_owner","text":"x"}}"#)
            .await;
        answer(&mut events, ActionResult::done("noted")).await;
        assert_eq!(client.next().await.unwrap()["result"]["code"], "done");
    }

    #[tokio::test]
    async fn a_line_over_the_limit_closes_the_connection() {
        let (server, _events) = server();
        let mut client = Client::connect(&server);
        client.send(&"x".repeat(MAX_LINE + 1)).await;
        assert_eq!(client.next().await, Some(json!({"error": "invalid"})));
        assert_eq!(client.next().await, None);
    }

    #[tokio::test]
    async fn without_a_running_bridge_actions_are_unavailable() {
        let server = ToolsServer::new(TOKEN, ActionSink::new());
        let mut client = Client::connect(&server);
        client
            .send(r#"{"token":"abc123","action":{"action":"outgoing"}}"#)
            .await;
        assert_eq!(
            client.next().await.unwrap()["result"]["code"],
            "unavailable"
        );
        let sink = ActionSink::new();
        let (events, receiver) = mpsc::unbounded_channel();
        sink.attach(events);
        drop(receiver);
        assert_eq!(
            sink.run(Action::Outgoing).await.code,
            ActionCode::Unavailable
        );
    }

    /// The real transport: a same-user pipe (Windows) or socket (Unix).
    #[tokio::test]
    async fn the_endpoint_serves_over_the_real_transport() {
        let id = format!("tt-{}", &crate::tools::new_token()[..12]);
        #[cfg(unix)]
        let root = crate::test_dir::TestDir::new();
        #[cfg(unix)]
        let endpoint = push_bridge_control::endpoint_for(&id, Some(root.path().as_os_str()));
        #[cfg(windows)]
        let endpoint = push_bridge_control::endpoint_for(&id, None);
        let endpoint = endpoint.unwrap();
        let listener = Listener::bind(&endpoint).unwrap();
        let (server, mut events) = server();
        let serving = server.serve(listener);
        let stream = push_bridge_control::transport::connect_same_user(&endpoint)
            .await
            .unwrap();
        let (reader, mut writer) = tokio::io::split(stream);
        let mut lines = BufReader::new(reader).lines();
        writer
            .write_all(b"{\"token\":\"abc123\",\"action\":{\"action\":\"outgoing\"}}\n")
            .await
            .unwrap();
        answer(&mut events, ActionResult::done("nothing waits")).await;
        let line = tokio::time::timeout(WAIT, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap(),
            json!({"result": {"code": "done", "detail": "nothing waits"}})
        );
        writer.write_all(b"{\"token\":\"nope\"}\n").await.unwrap();
        let line = tokio::time::timeout(WAIT, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(line, r#"{"error":"unauthorized"}"#);
        serving.abort();
    }
}
