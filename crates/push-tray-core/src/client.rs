//! The tray's side of a bridge's control channel: reads the bridge's `hello`, routes each
//! response to its request by id and every event to a channel.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use push_bridge_control::{
    Call, ErrorBody, Event, EventLine, MAX_LINE, Request, Response, ServerLine, Status, encode,
    endpoint, instance_id, transport,
};
use serde_json::Value;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// How long the bridge has to send its `hello`.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long [`ControlClient::status`] waits.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// Events the reader may queue before it waits for the receiver.
const EVENT_BUFFER: usize = 64;

/// The bridge's `hello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub protocol: u32,
    pub bridge: String,
    pub pid: u32,
    pub instance: String,
}

/// The requests waiting for their response; `None` once the connection is gone.
type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Response>>>>>;

/// A connection to one bridge. Dropping it closes the connection (and ends the event
/// channel).
pub struct ControlClient {
    hello: Hello,
    next_id: AtomicU64,
    pending: Pending,
    writes: mpsc::UnboundedSender<String>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl std::fmt::Debug for ControlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlClient")
            .field("hello", &self.hello)
            .finish_non_exhaustive()
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

/// Connects to the bridge that serves `dir` (its endpoint by instance id) and checks that
/// it is that instance. Nobody listening is [`io::ErrorKind::NotFound`] (also on Unix
/// without `XDG_RUNTIME_DIR`, where no bridge listens).
pub async fn connect(dir: &Path) -> io::Result<(ControlClient, mpsc::Receiver<EventLine>)> {
    let id = instance_id(dir);
    let Some(endpoint) = endpoint(&id) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no control endpoint: XDG_RUNTIME_DIR is not set",
        ));
    };
    let stream = transport::connect(&endpoint).await?;
    let (client, events) = connect_stream(stream).await?;
    if client.hello.instance != id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the bridge on this endpoint is another instance",
        ));
    }
    Ok((client, events))
}

/// A client over any stream (tests use `tokio::io::duplex`): waits for `hello` (at most
/// [`HELLO_TIMEOUT`]), then reads in the background.
pub async fn connect_stream<S>(stream: S) -> io::Result<(ControlClient, mpsc::Receiver<EventLine>)>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    let mut reader = tokio::io::BufReader::new(reader);
    let hello = tokio::time::timeout(HELLO_TIMEOUT, read_hello(&mut reader))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the bridge did not say hello"))??;
    let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
    let (event_sender, events) = mpsc::channel(EVENT_BUFFER);
    let reader = tokio::spawn(read_loop(reader, Arc::clone(&pending), event_sender));
    let (writes, lines) = mpsc::unbounded_channel();
    let writer = tokio::spawn(write_loop(writer, lines));
    Ok((
        ControlClient {
            hello,
            next_id: AtomicU64::new(1),
            pending,
            writes,
            reader,
            writer,
        },
        events,
    ))
}

impl ControlClient {
    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Sends `call` and waits for its response: the result, the bridge's error, or the
    /// local errors `timeout` and `disconnected`.
    pub async fn call(&self, call: Call, timeout: Duration) -> Result<Value, ErrorBody> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, answer) = oneshot::channel();
        match lock(&self.pending).as_mut() {
            Some(pending) => {
                pending.insert(id, sender);
            }
            None => return Err(disconnected()),
        }
        if self.writes.send(encode(&Request { id, call })).is_err() {
            self.forget(id);
            return Err(disconnected());
        }
        match tokio::time::timeout(timeout, answer).await {
            Ok(Ok(response)) => into_result(response),
            Ok(Err(_)) => Err(disconnected()),
            Err(_) => {
                self.forget(id);
                Err(local_error("timeout", "the bridge did not answer in time"))
            }
        }
    }

    /// The bridge's `status` (at most [`STATUS_TIMEOUT`]).
    pub async fn status(&self) -> Result<Status, ErrorBody> {
        let value = self.call(Call::Status, STATUS_TIMEOUT).await?;
        serde_json::from_value(value)
            .map_err(|_| local_error("bad_response", "the status could not be read"))
    }

    fn forget(&self, id: u64) {
        if let Some(pending) = lock(&self.pending).as_mut() {
            pending.remove(&id);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn into_result(response: Response) -> Result<Value, ErrorBody> {
    if response.ok {
        Ok(response.result.unwrap_or(Value::Null))
    } else {
        Err(response
            .error
            .unwrap_or_else(|| local_error("error", "the bridge refused without a reason")))
    }
}

fn local_error(code: &str, message: &str) -> ErrorBody {
    ErrorBody {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn disconnected() -> ErrorBody {
    local_error("disconnected", "the connection to the bridge is closed")
}

async fn read_hello<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Hello> {
    let Some(line) = read_line(reader).await? else {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the bridge closed the connection before hello",
        ));
    };
    match serde_json::from_str::<ServerLine>(&line) {
        Ok(ServerLine::Event(EventLine {
            event:
                Event::Hello {
                    protocol,
                    bridge,
                    pid,
                    instance,
                },
            ..
        })) => Ok(Hello {
            protocol,
            bridge,
            pid,
            instance,
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the first line is not hello",
        )),
    }
}

/// The next non-blank line without its line end; `None` at the end. A line over
/// [`MAX_LINE`] is an error.
async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<String>> {
    let limit = u64::try_from(MAX_LINE).unwrap_or(u64::MAX) + 2;
    loop {
        let mut buffer = Vec::new();
        if (&mut *reader)
            .take(limit)
            .read_until(b'\n', &mut buffer)
            .await?
            == 0
        {
            return Ok(None);
        }
        if buffer.last() == Some(&b'\n') {
            buffer.pop();
            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }
        }
        if buffer.len() > MAX_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the bridge sent a line that is too long",
            ));
        }
        if !buffer.iter().all(u8::is_ascii_whitespace) {
            return Ok(Some(String::from_utf8_lossy(&buffer).into_owned()));
        }
    }
}

async fn read_loop<R: AsyncBufRead + Unpin>(
    mut reader: R,
    pending: Pending,
    events: mpsc::Sender<EventLine>,
) {
    while let Ok(Some(line)) = read_line(&mut reader).await {
        match serde_json::from_str::<ServerLine>(&line) {
            Ok(ServerLine::Response(response)) => {
                let waiter = lock(&pending)
                    .as_mut()
                    .and_then(|pending| pending.remove(&response.id));
                if let Some(waiter) = waiter {
                    let _ = waiter.send(response);
                }
            }
            Ok(ServerLine::Event(event)) => {
                let _ = events.send(event).await;
            }
            // A newer bridge's event or field this tray does not know.
            Err(_) => tracing::debug!(event = "control_line_unknown"),
        }
    }
    // Dropping the waiters answers them `disconnected`.
    lock(&pending).take();
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut lines: mpsc::UnboundedReceiver<String>,
) {
    while let Some(line) = lines.recv().await {
        if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use push_bridge_control::{AgentInfo, BridgeState, ServerLine, parse_request};
    use serde_json::json;
    use tokio::io::{BufReader, DuplexStream, Lines, ReadHalf, WriteHalf};

    /// The bridge's end of a duplex stream.
    struct FakeBridge {
        lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
        writer: WriteHalf<DuplexStream>,
    }

    impl FakeBridge {
        async fn send(&mut self, line: &str) {
            self.writer.write_all(line.as_bytes()).await.unwrap();
            self.writer.write_all(b"\n").await.unwrap();
        }

        async fn event(&mut self, event: Event) {
            let line = encode(&EventLine { event, at: 7 });
            self.writer.write_all(line.as_bytes()).await.unwrap();
        }

        async fn hello(&mut self) {
            self.event(Event::Hello {
                protocol: 1,
                bridge: "0.1.0".into(),
                pid: 42,
                instance: "00112233aabbccdd".into(),
            })
            .await;
        }

        async fn request(&mut self) -> Request {
            let line = self.lines.next_line().await.unwrap().unwrap();
            parse_request(&line).unwrap()
        }

        async fn respond(&mut self, response: Response) {
            let line = encode(&ServerLine::Response(response));
            self.writer.write_all(line.as_bytes()).await.unwrap();
        }
    }

    fn pair() -> (DuplexStream, FakeBridge) {
        let (client, server) = tokio::io::duplex(MAX_LINE * 4);
        let (reader, writer) = tokio::io::split(server);
        (
            client,
            FakeBridge {
                lines: BufReader::new(reader).lines(),
                writer,
            },
        )
    }

    fn ok(id: u64, result: Value) -> Response {
        Response {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    #[tokio::test]
    async fn hello_comes_first_and_events_reach_the_channel() {
        let (stream, mut bridge) = pair();
        bridge.send("").await;
        bridge.hello().await;
        bridge
            .event(Event::StateChanged {
                state: BridgeState::Waiting,
                detail: Some("approval".into()),
            })
            .await;
        bridge.send(r#"{"event":"from_the_future","at":1}"#).await;
        bridge.event(Event::TurnEnded { interrupted: false }).await;
        let (client, mut events) = connect_stream(stream).await.unwrap();
        assert_eq!(client.hello().pid, 42);
        assert_eq!(client.hello().instance, "00112233aabbccdd");
        assert_eq!(
            events.recv().await.unwrap().event,
            Event::StateChanged {
                state: BridgeState::Waiting,
                detail: Some("approval".into()),
            }
        );
        assert_eq!(
            events.recv().await.unwrap().event,
            Event::TurnEnded { interrupted: false }
        );
    }

    #[tokio::test]
    async fn a_first_line_that_is_not_hello_is_refused() {
        let (stream, mut bridge) = pair();
        bridge.event(Event::TurnEnded { interrupted: false }).await;
        let error = connect_stream(stream).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let (stream, bridge) = pair();
        drop(bridge);
        let error = connect_stream(stream).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_bridge_times_out_before_hello() {
        let (stream, _bridge) = pair();
        let error = connect_stream(stream).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn responses_find_their_request_by_id_in_any_order() {
        let (stream, mut bridge) = pair();
        bridge.hello().await;
        let (client, _events) = connect_stream(stream).await.unwrap();
        let client = Arc::new(client);
        let first = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.call(Call::Reconnect, Duration::from_secs(5)).await }
        });
        let one = bridge.request().await;
        assert_eq!(one.call, Call::Reconnect);
        let second = tokio::spawn({
            let client = Arc::clone(&client);
            async move { client.status().await }
        });
        let two = bridge.request().await;
        assert_eq!(two.call, Call::Status);
        assert_ne!(one.id, two.id);
        let status = Status {
            instance: "00112233aabbccdd".into(),
            agent: Some(AgentInfo {
                display_name: "Coda".into(),
                handle: "coda".into(),
            }),
            ..Status::default()
        };
        bridge
            .respond(ok(two.id, serde_json::to_value(&status).unwrap()))
            .await;
        bridge
            .respond(Response {
                id: one.id,
                ok: false,
                result: None,
                error: Some(ErrorBody {
                    code: "busy".into(),
                    message: "a pause is running".into(),
                }),
            })
            .await;
        assert_eq!(second.await.unwrap().unwrap(), status);
        assert_eq!(first.await.unwrap().unwrap_err().code, "busy");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_call_times_out_and_a_late_answer_is_dropped() {
        let (stream, mut bridge) = pair();
        bridge.hello().await;
        let (client, _events) = connect_stream(stream).await.unwrap();
        let error = client
            .call(Call::Shutdown, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(error.code, "timeout");
        let late = bridge.request().await;
        bridge.respond(ok(late.id, json!({}))).await;
        let call =
            tokio::spawn(async move { client.call(Call::Reconnect, Duration::from_secs(5)).await });
        let next = bridge.request().await;
        bridge.respond(ok(next.id, json!({"done": true}))).await;
        assert_eq!(call.await.unwrap().unwrap(), json!({"done": true}));
    }

    #[tokio::test]
    async fn a_closed_connection_answers_disconnected_and_ends_the_events() {
        let (stream, mut bridge) = pair();
        bridge.hello().await;
        let (client, mut events) = connect_stream(stream).await.unwrap();
        let call = tokio::spawn(async move {
            let first = client.call(Call::Reconnect, Duration::from_secs(60)).await;
            let after = client.call(Call::Reconnect, Duration::from_secs(60)).await;
            (first, after)
        });
        bridge.request().await;
        drop(bridge);
        let (first, after) = call.await.unwrap();
        assert_eq!(first.unwrap_err().code, "disconnected");
        assert_eq!(after.unwrap_err().code, "disconnected");
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn dropping_the_client_closes_the_connection() {
        let (stream, mut bridge) = pair();
        bridge.hello().await;
        let (client, mut events) = connect_stream(stream).await.unwrap();
        drop(client);
        assert!(events.recv().await.is_none());
        assert!(bridge.lines.next_line().await.unwrap().is_none());
    }
}
