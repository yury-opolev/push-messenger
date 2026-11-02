//! The local control channel's server side: the [`ControlHub`] serves the tray's
//! connections, answers `status` from the bridge's snapshot, forwards every event the
//! core emits, and hands the actions to the bridge as [`ControlCall`]s.
//!
//! Everything that goes out here is content-free (see `push_bridge_control::protocol`):
//! states, counts, names and error codes, never message text.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use push_bridge_control::transport::Listener;
use push_bridge_control::{
    BridgeState, Call, ErrorBody, Event, EventLine, MAX_LINE, PROTOCOL, PauseParams, Response,
    Status, encode, now_ms, parse_request,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

/// Connections served at once; more are closed at once.
pub const MAX_CONNECTIONS: usize = 8;
/// How long an action may take before its caller hears `timeout`.
const ACTION_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the backend may drain when `pause_backend` does not say.
const DEFAULT_PAUSE_DRAIN: Duration = Duration::from_secs(10 * 60);
/// How much longer than its drain `pause_backend` may take (saving, stopping).
const PAUSE_GRACE: Duration = Duration::from_secs(2 * 60);
/// Events a slow connection may fall behind before it skips some.
const EVENT_BUFFER: usize = 256;
/// Actions waiting for the bridge.
const CALL_BUFFER: usize = 16;

/// What a client asks the bridge to do (`status` is answered by the hub itself).
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    RestartBackend,
    Reconnect,
    Shutdown,
    Pause(PauseParams),
    Resume,
}

impl Action {
    /// The method's wire name, for logs.
    pub fn name(&self) -> &'static str {
        match self {
            Action::RestartBackend => "restart_backend",
            Action::Reconnect => "reconnect",
            Action::Shutdown => "shutdown",
            Action::Pause(_) => "pause_backend",
            Action::Resume => "resume_backend",
        }
    }
}

/// An action's result: the response's `result`, or its error.
pub type Reply = Result<Value, ErrorBody>;

/// An action for the bridge and where its result goes.
pub struct ControlCall {
    pub action: Action,
    pub reply: oneshot::Sender<Reply>,
}

/// The actions' receiver, shared by whoever serves them in turn (the sign-in loop, then
/// each bridge run).
pub type Calls = Arc<Mutex<mpsc::Receiver<ControlCall>>>;

/// An error body with `code` and `message`.
pub fn error(code: &str, message: impl Into<String>) -> ErrorBody {
    ErrorBody {
        code: code.to_owned(),
        message: message.into(),
    }
}

/// The bridge's side of the hub: the status snapshot and the events.
#[derive(Clone)]
pub struct ControlHandle {
    status: Arc<watch::Sender<Status>>,
    events: broadcast::Sender<EventLine>,
}

impl ControlHandle {
    fn new(status: Status) -> Self {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Self {
            status: Arc::new(watch::Sender::new(status)),
            events,
        }
    }

    /// A handle with no hub (for tests and `"control": false`): updates go nowhere.
    pub fn detached() -> Self {
        Self::new(Status::default())
    }

    /// Changes the status snapshot.
    pub fn update(&self, change: impl FnOnce(&mut Status)) {
        self.status.send_if_modified(|status| {
            let before = status.clone();
            change(status);
            *status != before
        });
    }

    /// Sends `event` to every connection, stamped with the time.
    pub fn emit(&self, event: Event) {
        let _ = self.events.send(EventLine {
            event,
            at: now_ms(),
        });
    }

    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    /// The events from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<EventLine> {
        self.events.subscribe()
    }
}

/// Serves the control channel: connections, `status`, events, and the actions the
/// bridge takes from [`ControlHub::calls`].
#[derive(Clone)]
pub struct ControlHub {
    handle: ControlHandle,
    calls: Calls,
    sender: mpsc::Sender<ControlCall>,
    connections: Arc<AtomicUsize>,
}

impl ControlHub {
    /// A hub for instance `instance`, starting.
    pub fn new(instance: String) -> Self {
        let handle = ControlHandle::new(Status {
            protocol: PROTOCOL,
            instance,
            pid: std::process::id(),
            bridge_version: env!("CARGO_PKG_VERSION").to_owned(),
            state: Some(BridgeState::Starting),
            started_at: now_ms(),
            ..Status::default()
        });
        let (sender, receiver) = mpsc::channel(CALL_BUFFER);
        Self {
            handle,
            calls: Arc::new(Mutex::new(receiver)),
            sender,
            connections: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn handle(&self) -> ControlHandle {
        self.handle.clone()
    }

    pub fn calls(&self) -> Calls {
        Arc::clone(&self.calls)
    }

    /// Hands `action` to the bridge and waits for its reply (no time limit), as a
    /// connection would; for in-process callers and tests.
    pub async fn call(&self, action: Action) -> Reply {
        let (reply, answer) = oneshot::channel();
        if self
            .sender
            .send(ControlCall { action, reply })
            .await
            .is_err()
        {
            return Err(unavailable());
        }
        answer.await.unwrap_or_else(|_| Err(unavailable()))
    }

    /// Serves `listener`'s clients until the task is aborted.
    pub fn serve(&self, mut listener: Listener) -> JoinHandle<()> {
        let hub = self.clone();
        tokio::spawn(async move {
            let mut refused = 0;
            loop {
                match listener.accept().await {
                    Ok(stream) => {
                        hub.serve_stream(stream);
                    }
                    Err(error) => {
                        tracing::warn!(event = "control_accept_failed", kind = ?error.kind());
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
                if listener.refused() != refused {
                    refused = listener.refused();
                    tracing::warn!(event = "control_peer_refused", total = refused);
                }
            }
        })
    }

    /// Serves one connected stream: `hello`, the current state, then events and
    /// responses until the client goes, sends a line over [`MAX_LINE`], or more than
    /// [`MAX_CONNECTIONS`] are open (closed at once).
    pub fn serve_stream<S>(&self, stream: S) -> JoinHandle<()>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let open = Open::new(Arc::clone(&self.connections));
        if open.count > MAX_CONNECTIONS {
            tracing::warn!(event = "control_connection_refused", open = open.count - 1);
            return tokio::spawn(async move {
                drop((open, stream));
            });
        }
        let hub = self.clone();
        tokio::spawn(async move {
            let _open = open;
            hub.connection(stream).await;
        })
    }

    async fn connection<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        tracing::debug!(event = "control_connected");
        let mut events = self.handle.subscribe();
        let (reader, mut writer) = tokio::io::split(stream);
        let (line_sender, mut lines) = mpsc::channel(4);
        let reading = tokio::spawn(read_lines(reader, line_sender));
        let (reply_sender, mut replies) = mpsc::unbounded_channel();
        let status = self.handle.status();
        let hello = Event::Hello {
            protocol: PROTOCOL,
            bridge: env!("CARGO_PKG_VERSION").to_owned(),
            pid: std::process::id(),
            instance: status.instance.clone(),
        };
        let mut first = vec![encode(&EventLine {
            event: hello,
            at: now_ms(),
        })];
        if let Some(state) = status.state {
            first.push(encode(&EventLine {
                event: Event::StateChanged {
                    state,
                    detail: status.detail,
                },
                at: now_ms(),
            }));
        }
        let mut ok = write_all(&mut writer, &first.concat()).await;
        while ok {
            ok = tokio::select! {
                line = lines.recv() => match line {
                    Some(Line::Text(text)) => match self.request(&text, &reply_sender) {
                        Some(response) => write_all(&mut writer, &encode(&response)).await,
                        None => true,
                    },
                    Some(Line::TooLong) => {
                        tracing::warn!(event = "control_line_too_long");
                        false
                    }
                    None => false,
                },
                Some(response) = replies.recv() => {
                    write_all(&mut writer, &encode(&response)).await
                }
                event = events.recv() => match event {
                    Ok(event) => write_all(&mut writer, &encode(&event)).await,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!(event = "control_events_skipped", skipped);
                        true
                    }
                    Err(broadcast::error::RecvError::Closed) => false,
                },
            };
        }
        reading.abort();
        tracing::debug!(event = "control_disconnected");
    }

    /// Answers `status` (and bad lines) at once; hands an action to the bridge and
    /// answers through `replies` when it is done.
    fn request(&self, line: &str, replies: &mpsc::UnboundedSender<Response>) -> Option<Response> {
        let request = match parse_request(line) {
            Ok(request) => request,
            Err((id, error)) => return Some(response(id.unwrap_or(0), Err(error))),
        };
        let id = request.id;
        let (action, limit) = match request.call {
            Call::Status => {
                let status = serde_json::to_value(self.handle.status())
                    .map_err(|_| error("internal", "the status could not be encoded"));
                return Some(response(id, status));
            }
            Call::RestartBackend => (Action::RestartBackend, ACTION_TIMEOUT),
            Call::Reconnect => (Action::Reconnect, ACTION_TIMEOUT),
            Call::Shutdown => (Action::Shutdown, ACTION_TIMEOUT),
            Call::PauseBackend(params) => {
                let limit = pause_timeout(&params);
                (Action::Pause(params), limit)
            }
            Call::ResumeBackend => (Action::Resume, ACTION_TIMEOUT),
        };
        tracing::info!(event = "control_request", method = action.name());
        let hub = self.clone();
        let replies = replies.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(limit, hub.call(action))
                .await
                .unwrap_or_else(|_| Err(error("timeout", "the bridge did not answer in time")));
            let _ = replies.send(response(id, result));
        });
        None
    }
}

fn unavailable() -> ErrorBody {
    error("unavailable", "the bridge is not taking requests")
}

/// How long a `pause_backend` call may take: its drain (10 minutes when it does not say)
/// and a grace, however long the drain.
fn pause_timeout(params: &PauseParams) -> Duration {
    params
        .drain_timeout_ms
        .map_or(DEFAULT_PAUSE_DRAIN, Duration::from_millis)
        .saturating_add(PAUSE_GRACE)
}

fn response(id: u64, result: Reply) -> Response {
    match result {
        Ok(result) => Response {
            id,
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(error) => Response {
            id,
            ok: false,
            result: None,
            error: Some(error),
        },
    }
}

/// Counts an open connection while it lives; `count` includes it.
struct Open {
    connections: Arc<AtomicUsize>,
    count: usize,
}

impl Open {
    fn new(connections: Arc<AtomicUsize>) -> Self {
        let count = connections.fetch_add(1, Ordering::SeqCst) + 1;
        Self { connections, count }
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

enum Line {
    Text(String),
    TooLong,
}

/// Reads lines (without their `\n`) until the end, an error, or a line over
/// [`MAX_LINE`]; blank lines are skipped.
async fn read_lines<R: AsyncRead + Unpin>(reader: R, lines: mpsc::Sender<Line>) {
    let mut reader = BufReader::new(reader);
    let limit = u64::try_from(MAX_LINE).unwrap_or(u64::MAX) + 1;
    loop {
        let mut buffer = Vec::new();
        let read = (&mut reader)
            .take(limit)
            .read_until(b'\n', &mut buffer)
            .await;
        let ended = match read {
            Ok(0) | Err(_) => return,
            Ok(_) => buffer.last() != Some(&b'\n'),
        };
        if buffer.last() == Some(&b'\n') {
            buffer.pop();
            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }
        }
        if buffer.len() > MAX_LINE {
            let _ = lines.send(Line::TooLong).await;
            return;
        }
        if !buffer.iter().all(u8::is_ascii_whitespace) {
            let text = String::from_utf8_lossy(&buffer).into_owned();
            if lines.send(Line::Text(text)).await.is_err() {
                return;
            }
        }
        if ended {
            return;
        }
    }
}

async fn write_all<W: AsyncWrite + Unpin>(writer: &mut W, text: &str) -> bool {
    writer.write_all(text.as_bytes()).await.is_ok() && writer.flush().await.is_ok()
}

#[cfg(test)]
mod tests {
    use push_bridge_control::ServerLine;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf};

    use super::*;

    struct Client {
        lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
        writer: WriteHalf<DuplexStream>,
    }

    impl Client {
        fn connect(hub: &ControlHub) -> Self {
            let (client, server) = tokio::io::duplex(MAX_LINE * 4);
            hub.serve_stream(server);
            let (reader, writer) = tokio::io::split(client);
            Self {
                lines: BufReader::new(reader).lines(),
                writer,
            }
        }

        async fn next(&mut self) -> Option<ServerLine> {
            let line = self.lines.next_line().await.ok()??;
            Some(serde_json::from_str(&line).expect("a server line"))
        }

        async fn send(&mut self, line: &str) {
            self.writer
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
        }

        /// The next response, skipping events.
        async fn response(&mut self) -> Response {
            loop {
                match self.next().await.expect("a response") {
                    ServerLine::Response(response) => return response,
                    ServerLine::Event(_) => {}
                }
            }
        }

        async fn greeted(hub: &ControlHub) -> Self {
            let mut client = Self::connect(hub);
            client.next().await;
            client.next().await;
            client
        }
    }

    fn hub() -> ControlHub {
        ControlHub::new("0011223344556677".into())
    }

    #[tokio::test]
    async fn hello_comes_first_then_the_state() {
        let hub = hub();
        let mut client = Client::connect(&hub);
        let Some(ServerLine::Event(hello)) = client.next().await else {
            panic!("hello");
        };
        assert_eq!(
            hello.event,
            Event::Hello {
                protocol: PROTOCOL,
                bridge: env!("CARGO_PKG_VERSION").into(),
                pid: std::process::id(),
                instance: "0011223344556677".into(),
            }
        );
        let Some(ServerLine::Event(state)) = client.next().await else {
            panic!("state");
        };
        assert_eq!(
            state.event,
            Event::StateChanged {
                state: BridgeState::Starting,
                detail: None
            }
        );
        hub.handle().emit(Event::TurnEnded { interrupted: false });
        let Some(ServerLine::Event(event)) = client.next().await else {
            panic!("an event");
        };
        assert_eq!(event.event, Event::TurnEnded { interrupted: false });
    }

    #[tokio::test]
    async fn status_is_answered_from_the_snapshot() {
        let hub = hub();
        hub.handle().update(|status| {
            status.state = Some(BridgeState::Idle);
            status.open_requests = 2;
        });
        let mut client = Client::greeted(&hub).await;
        client.send(r#"{"id":7,"method":"status"}"#).await;
        let response = client.response().await;
        assert_eq!((response.id, response.ok), (7, true));
        let status: Status = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(status.state, Some(BridgeState::Idle));
        assert_eq!(status.open_requests, 2);
        assert_eq!(status.instance, "0011223344556677");
        assert_eq!(status.pid, std::process::id());
    }

    #[tokio::test]
    async fn actions_go_to_the_bridge_and_its_reply_comes_back() {
        let hub = hub();
        let calls = hub.calls();
        let bridge = tokio::spawn(async move {
            let call = calls.lock().await.recv().await.expect("a call");
            assert_eq!(call.action, Action::RestartBackend);
            let _ = call.reply.send(Ok(json!({"restarted": true})));
        });
        let mut client = Client::greeted(&hub).await;
        client
            .send(r#"{"id":3,"method":"restart_backend","params":{}}"#)
            .await;
        let response = client.response().await;
        assert_eq!(
            response,
            Response {
                id: 3,
                ok: true,
                result: Some(json!({"restarted": true})),
                error: None
            }
        );
        bridge.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_action_times_out() {
        let hub = hub();
        let mut client = Client::greeted(&hub).await;
        client.send(r#"{"id":4,"method":"reconnect"}"#).await;
        let response = client.response().await;
        assert_eq!(response.id, 4);
        assert_eq!(response.error.unwrap().code, "timeout");
    }

    #[test]
    fn a_pause_may_take_its_drain_and_a_grace() {
        let pause = |drain_timeout_ms| PauseParams {
            reason: None,
            drain_timeout_ms,
        };
        assert_eq!(pause_timeout(&pause(None)), Duration::from_secs(12 * 60));
        assert_eq!(
            pause_timeout(&pause(Some(30 * 60 * 1000))),
            Duration::from_secs(32 * 60)
        );
        assert_eq!(
            pause_timeout(&pause(Some(u64::MAX))),
            Duration::from_millis(u64::MAX) + PAUSE_GRACE
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_drain_is_answered_after_the_old_fifteen_minutes() {
        let hub = hub();
        let calls = hub.calls();
        let bridge = tokio::spawn(async move {
            let call = calls.lock().await.recv().await.expect("a call");
            tokio::time::sleep(Duration::from_secs(20 * 60)).await;
            let _ = call.reply.send(Ok(json!({"state": "paused"})));
        });
        let mut client = Client::greeted(&hub).await;
        client
            .send(r#"{"id":5,"method":"pause_backend","params":{"drainTimeoutMs":1200000}}"#)
            .await;
        let response = client.response().await;
        assert_eq!(
            (response.id, response.result),
            (5, Some(json!({"state": "paused"})))
        );
        bridge.await.unwrap();
    }

    #[tokio::test]
    async fn a_bad_line_gets_an_error_and_the_connection_stays() {
        let hub = hub();
        let mut client = Client::greeted(&hub).await;
        client.send("not json").await;
        let response = client.response().await;
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "bad_request");
        client.send(r#"{"id":9,"method":"nope"}"#).await;
        let response = client.response().await;
        assert_eq!(
            (response.id, response.error.unwrap().code.as_str()),
            (9, "unknown_method")
        );
        client.send(r#"{"id":10,"method":"status"}"#).await;
        assert!(client.response().await.ok);
    }

    #[tokio::test]
    async fn an_over_long_line_closes_the_connection() {
        let hub = hub();
        let mut client = Client::greeted(&hub).await;
        let long = "x".repeat(MAX_LINE + 10);
        let _ = client.writer.write_all(long.as_bytes()).await;
        assert!(client.next().await.is_none(), "closed");
    }

    #[tokio::test]
    async fn at_most_eight_connections_are_served() {
        let hub = hub();
        let mut clients = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            clients.push(Client::greeted(&hub).await);
        }
        let mut extra = Client::connect(&hub);
        assert!(extra.next().await.is_none(), "closed at once");
        drop(clients.pop());
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut again = Client::connect(&hub);
        assert!(again.next().await.is_some(), "served again");
    }
}
