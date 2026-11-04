//! The local control channel's server side: the [`ControlHub`] serves the tray's
//! connections, answers `status` from the bridge's snapshot, forwards every event the
//! core emits, and hands the actions to the bridge as [`ControlCall`]s.
//!
//! Everything that goes out here is content-free (see `push_bridge_control::protocol`):
//! states, counts, names and error codes, never message text. The one exception is the
//! `peek` response: the owner conversation's last messages, masked, from the in-memory
//! [`PeekRing`] (off with `"peek": false`); its `peek_changed` event carries nothing.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use push_bridge_control::transport::Listener;
use push_bridge_control::{
    BridgeState, Call, CallStateResult, ErrorBody, Event, EventLine, MAX_LINE, PEEK_DISABLED,
    PROTOCOL, PauseParams, PeekEntry, PeekResult, Response, Status, encode, now_ms, parse_request,
    peek_limit,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::peek::PeekRing;

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
    /// Mutes (`true`) or unmutes the agent's voice in the running call.
    Mute(bool),
    /// Hangs up, cancels or declines the call.
    Hangup,
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
            Action::Mute(_) => "mute",
            Action::Hangup => "hangup",
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

/// The chat peek shared by the handles: off, or the ring.
struct PeekShared {
    enabled: bool,
    ring: StdMutex<PeekRing>,
}

/// The bridge's side of the hub: the status snapshot, the events and the chat peek.
#[derive(Clone)]
pub struct ControlHandle {
    status: Arc<watch::Sender<Status>>,
    events: broadcast::Sender<EventLine>,
    peek: Arc<PeekShared>,
}

impl ControlHandle {
    fn new(status: Status, peek: bool) -> Self {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Self {
            status: Arc::new(watch::Sender::new(status)),
            events,
            peek: Arc::new(PeekShared {
                enabled: peek,
                ring: StdMutex::new(PeekRing::new()),
            }),
        }
    }

    /// A handle with no hub (for tests and `"control": false`): updates go nowhere and
    /// the peek is off.
    pub fn detached() -> Self {
        Self::new(Status::default(), false)
    }

    /// Changes the peek's ring, then emits `peek_changed` when its entries changed;
    /// nothing while the peek is off.
    pub fn record(&self, change: impl FnOnce(&mut PeekRing)) {
        if !self.peek.enabled {
            return;
        }
        let changed = {
            let mut ring = self
                .peek
                .ring
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let before = ring.revision();
            change(&mut ring);
            ring.revision() != before
        };
        if changed {
            self.emit(Event::PeekChanged);
        }
    }

    /// The peek's last `limit` entries, oldest first; `peek_disabled` while it is off.
    pub fn peek(&self, limit: usize) -> Result<Vec<PeekEntry>, ErrorBody> {
        if !self.peek.enabled {
            return Err(error(PEEK_DISABLED, "the chat peek is off in config.json"));
        }
        let ring = self
            .peek
            .ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(ring.last(limit))
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

    /// Changes the status snapshot, then sends `event` (if any). In this order: a
    /// connection subscribes before it reads the snapshot, so it sees the change in
    /// one or the other.
    pub fn update_and_emit(&self, change: impl FnOnce(&mut Status), event: Option<Event>) {
        self.update(change);
        if let Some(event) = event {
            self.emit(event);
        }
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
    /// A hub for instance `instance`, starting; `peek` turns the chat peek on.
    pub fn new(instance: String, peek: bool) -> Self {
        let handle = ControlHandle::new(
            Status {
                protocol: PROTOCOL,
                instance,
                pid: std::process::id(),
                bridge_version: env!("CARGO_PKG_VERSION").to_owned(),
                state: Some(BridgeState::Starting),
                started_at: now_ms(),
                ..Status::default()
            },
            peek,
        );
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
            Call::Peek(params) => {
                let entries = self.handle.peek(peek_limit(&params)).and_then(|entries| {
                    serde_json::to_value(PeekResult { entries })
                        .map_err(|_| error("internal", "the peek could not be encoded"))
                });
                return Some(response(id, entries));
            }
            Call::CallState => {
                let call = serde_json::to_value(CallStateResult {
                    call: self.handle.status().call,
                })
                .map_err(|_| error("internal", "the call state could not be encoded"));
                return Some(response(id, call));
            }
            Call::Mute(params) => (Action::Mute(params.muted.unwrap_or(true)), ACTION_TIMEOUT),
            Call::Hangup => (Action::Hangup, ACTION_TIMEOUT),
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
        ControlHub::new("0011223344556677".into(), true)
    }

    #[tokio::test]
    async fn peek_returns_the_recorded_entries_and_record_emits_peek_changed() {
        let hub = hub();
        let mut events = hub.handle().subscribe();
        for i in 0..5 {
            hub.handle()
                .record(|ring| ring.owner_text(i, &format!("m{i}")));
        }
        for _ in 0..5 {
            assert_eq!(events.try_recv().unwrap().event, Event::PeekChanged);
        }
        hub.handle().record(|_| {});
        assert!(events.try_recv().is_err(), "nothing changed");
        let mut client = Client::greeted(&hub).await;
        client
            .send(r#"{"id":9,"method":"peek","params":{"limit":3}}"#)
            .await;
        let response = client.response().await;
        assert_eq!((response.id, response.ok), (9, true));
        let result: PeekResult = serde_json::from_value(response.result.unwrap()).unwrap();
        let texts: Vec<_> = result.entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["m2", "m3", "m4"]);
        client.send(r#"{"id":10,"method":"peek"}"#).await;
        let result: PeekResult =
            serde_json::from_value(client.response().await.result.unwrap()).unwrap();
        assert_eq!(result.entries.len(), 5);
    }

    #[tokio::test]
    async fn a_hub_without_the_peek_records_nothing_and_answers_peek_disabled() {
        let hub = ControlHub::new("0011223344556677".into(), false);
        let mut events = hub.handle().subscribe();
        let mut called = false;
        hub.handle().record(|ring| {
            called = true;
            ring.owner_text(1, "hi");
        });
        assert!(!called);
        assert!(events.try_recv().is_err());
        assert_eq!(hub.handle().peek(20).unwrap_err().code, PEEK_DISABLED);
        let mut client = Client::greeted(&hub).await;
        client.send(r#"{"id":4,"method":"peek"}"#).await;
        let response = client.response().await;
        assert_eq!(
            (response.id, response.error.unwrap().code.as_str()),
            (4, PEEK_DISABLED)
        );
        assert!(ControlHandle::detached().peek(20).is_err());
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

    #[test]
    fn the_snapshot_changes_before_the_event_goes_out() {
        let handle = ControlHandle::detached();
        let mut events = handle.subscribe();
        // Holding the snapshot blocks its change: the event must wait for it.
        let held = handle.status.borrow();
        let writer = {
            let handle = handle.clone();
            std::thread::spawn(move || {
                handle.update_and_emit(
                    |status| status.state = Some(BridgeState::Working),
                    Some(Event::StateChanged {
                        state: BridgeState::Working,
                        detail: None,
                    }),
                );
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            events.try_recv().is_err(),
            "the event went out before the snapshot changed"
        );
        drop(held);
        writer.join().unwrap();
        let line = events.try_recv().expect("the event");
        assert!(matches!(
            line.event,
            Event::StateChanged {
                state: BridgeState::Working,
                ..
            }
        ));
        assert_eq!(handle.status().state, Some(BridgeState::Working));
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
