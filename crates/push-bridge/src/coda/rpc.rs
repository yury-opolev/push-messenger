//! A JSON-RPC 2.0 peer over a framed reader/writer pair: calls with numeric ids,
//! responses matched by id, and the other side's notifications and requests delivered
//! as [`Incoming`] messages.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};

use super::framing::{FrameReader, write_frame};

/// What the other side sent that is not a response.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// Must be answered with [`RpcPeer::respond`] or [`RpcPeer::respond_error`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// The connection closed; sent once, last.
    Closed {
        reason: String,
    },
}

/// A failed call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RpcError {
    #[error("{message} ({code})")]
    Remote { code: i64, message: String },
    #[error("the connection closed")]
    Closed,
    #[error("no answer in time")]
    Timeout,
}

type Pending = HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>;

struct Inner {
    next_id: AtomicU64,
    /// `None` once closed.
    pending: Mutex<Option<Pending>>,
    outgoing: mpsc::UnboundedSender<Value>,
    /// `None` once closed, so the receiver ends after [`Incoming::Closed`].
    incoming: Mutex<Option<mpsc::UnboundedSender<Incoming>>>,
    closed: watch::Sender<bool>,
}

impl Inner {
    fn pending(&self) -> MutexGuard<'_, Option<Pending>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn close(&self, reason: &str) {
        let Some(pending) = self.pending().take() else {
            return;
        };
        for (_, sender) in pending {
            let _ = sender.send(Err(RpcError::Closed));
        }
        self.closed.send_replace(true);
        let incoming = self
            .incoming
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(incoming) = incoming {
            let _ = incoming.send(Incoming::Closed {
                reason: reason.to_owned(),
            });
        }
    }

    fn dispatch(&self, mut message: Value) {
        let Some(object) = message.as_object_mut() else {
            return;
        };
        let id = object.remove("id").filter(|id| !id.is_null());
        if let Some(Value::String(method)) = object.remove("method") {
            let params = object.remove("params").unwrap_or(Value::Null);
            let incoming = match id {
                Some(id) => Incoming::Request { id, method, params },
                None => Incoming::Notification { method, params },
            };
            let sender = self.incoming.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(sender) = sender.as_ref() {
                let _ = sender.send(incoming);
            }
            return;
        }
        let Some(id) = id.as_ref().and_then(Value::as_u64) else {
            return;
        };
        let Some(sender) = self.pending().as_mut().and_then(|p| p.remove(&id)) else {
            return;
        };
        let result = match object.remove("error") {
            Some(error) => Err(RpcError::Remote {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }),
            None => Ok(object.remove("result").unwrap_or(Value::Null)),
        };
        let _ = sender.send(result);
    }
}

/// One side of a JSON-RPC connection. Clones share the connection.
#[derive(Clone)]
pub struct RpcPeer {
    inner: Arc<Inner>,
}

impl RpcPeer {
    /// Starts the reader and writer tasks; the receiver gets everything that is not a
    /// response, ending with [`Incoming::Closed`].
    pub fn new<R, W>(reader: R, writer: W) -> (Self, mpsc::UnboundedReceiver<Incoming>)
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let (outgoing, outgoing_rx) = mpsc::unbounded_channel();
        let (incoming, incoming_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(Some(HashMap::new())),
            outgoing,
            incoming: Mutex::new(Some(incoming)),
            closed: watch::channel(false).0,
        });
        tokio::spawn(write_loop(Arc::clone(&inner), outgoing_rx, writer));
        tokio::spawn(read_loop(Arc::clone(&inner), reader));
        (Self { inner }, incoming_rx)
    }

    /// Calls `method`; `None` waits as long as the connection lives.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, RpcError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        match self.inner.pending().as_mut() {
            Some(pending) => pending.insert(id, sender),
            None => return Err(RpcError::Closed),
        };
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if self.inner.outgoing.send(frame).is_err() {
            self.forget(id);
            return Err(RpcError::Closed);
        }
        let answer = match timeout {
            Some(limit) => match tokio::time::timeout(limit, receiver).await {
                Ok(answer) => answer,
                Err(_) => {
                    self.forget(id);
                    return Err(RpcError::Timeout);
                }
            },
            None => receiver.await,
        };
        answer.unwrap_or(Err(RpcError::Closed))
    }

    /// Answers the other side's request.
    pub fn respond(&self, id: Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    /// Refuses the other side's request.
    pub fn respond_error(&self, id: Value, code: i64, message: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }));
    }

    /// Closes the connection: pending calls fail with [`RpcError::Closed`] and
    /// [`Incoming::Closed`] carries `reason`. Closing again does nothing.
    pub fn close(&self, reason: &str) {
        self.inner.close(reason);
    }

    pub fn is_closed(&self) -> bool {
        self.inner.pending().is_none()
    }

    /// Resolves once the connection is closed.
    pub async fn closed(&self) {
        let mut closed = self.inner.closed.subscribe();
        let _ = closed.wait_for(|closed| *closed).await;
    }

    fn send(&self, frame: Value) {
        if !self.is_closed() {
            let _ = self.inner.outgoing.send(frame);
        }
    }

    fn forget(&self, id: u64) {
        if let Some(pending) = self.inner.pending().as_mut() {
            pending.remove(&id);
        }
    }
}

/// Resolves once closed; drops the watch guard before returning.
async fn wait_closed(closed: &mut watch::Receiver<bool>) {
    let _ = closed.wait_for(|closed| *closed).await;
}

async fn write_loop<W: AsyncWrite + Unpin>(
    inner: Arc<Inner>,
    mut outgoing: mpsc::UnboundedReceiver<Value>,
    mut writer: W,
) {
    let mut closed = inner.closed.subscribe();
    loop {
        tokio::select! {
            biased;
            _ = wait_closed(&mut closed) => break,
            frame = outgoing.recv() => {
                let Some(frame) = frame else { break };
                if write_frame(&mut writer, &frame).await.is_err() {
                    inner.close("the connection broke");
                    break;
                }
            }
        }
    }
}

async fn read_loop<R: AsyncRead + Unpin>(inner: Arc<Inner>, reader: R) {
    let mut frames = FrameReader::new(reader);
    let mut closed = inner.closed.subscribe();
    loop {
        let frame = tokio::select! {
            biased;
            _ = wait_closed(&mut closed) => return,
            frame = frames.next() => frame,
        };
        match frame {
            Ok(Some(message)) => inner.dispatch(message),
            Ok(None) => return inner.close("the stream ended"),
            Err(error) => return inner.close(&format!("unreadable frame: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{DuplexStream, duplex};

    use super::*;

    /// The other side of the connection, scripted by the test.
    struct Script {
        frames: FrameReader<DuplexStream>,
        writer: DuplexStream,
    }

    impl Script {
        async fn read(&mut self) -> Value {
            self.frames.next().await.unwrap().expect("a frame")
        }

        async fn write(&mut self, value: Value) {
            write_frame(&mut self.writer, &value).await.unwrap();
        }
    }

    fn connect() -> (RpcPeer, mpsc::UnboundedReceiver<Incoming>, Script) {
        let (ours, theirs_in) = duplex(4096);
        let (theirs_out, ours_in) = duplex(4096);
        let (peer, incoming) = RpcPeer::new(ours_in, ours);
        let script = Script {
            frames: FrameReader::new(theirs_in),
            writer: theirs_out,
        };
        (peer, incoming, script)
    }

    #[tokio::test]
    async fn a_call_gets_its_response() {
        let (peer, _incoming, mut script) = connect();
        let call = tokio::spawn({
            let peer = peer.clone();
            async move { peer.call("ping", json!({"x": 1}), None).await }
        });
        let request = script.read().await;
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["method"], "ping");
        assert_eq!(request["params"], json!({"x": 1}));
        script
            .write(json!({"jsonrpc": "2.0", "id": request["id"], "result": {"pong": true}}))
            .await;
        assert_eq!(call.await.unwrap(), Ok(json!({"pong": true})));
    }

    #[tokio::test]
    async fn responses_are_matched_by_id() {
        let (peer, _incoming, mut script) = connect();
        let first = tokio::spawn({
            let peer = peer.clone();
            async move { peer.call("a", Value::Null, None).await }
        });
        let a = script.read().await;
        let second = tokio::spawn({
            let peer = peer.clone();
            async move { peer.call("b", Value::Null, None).await }
        });
        let b = script.read().await;
        script
            .write(json!({"jsonrpc": "2.0", "id": b["id"], "result": "b"}))
            .await;
        script
            .write(json!({"jsonrpc": "2.0", "id": a["id"], "result": "a"}))
            .await;
        assert_eq!(first.await.unwrap(), Ok(json!("a")));
        assert_eq!(second.await.unwrap(), Ok(json!("b")));
    }

    #[tokio::test]
    async fn an_error_response_fails_the_call() {
        let (peer, _incoming, mut script) = connect();
        let call = tokio::spawn({
            let peer = peer.clone();
            async move { peer.call("session/prompt", json!({}), None).await }
        });
        let request = script.read().await;
        script
            .write(json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "error": {"code": -32603, "message": "busy: a turn is already running"},
            }))
            .await;
        assert_eq!(
            call.await.unwrap(),
            Err(RpcError::Remote {
                code: -32603,
                message: "busy: a turn is already running".into()
            })
        );
    }

    #[tokio::test]
    async fn notifications_interleave_with_responses() {
        let (peer, mut incoming, mut script) = connect();
        let call = tokio::spawn({
            let peer = peer.clone();
            async move { peer.call("work", Value::Null, None).await }
        });
        let request = script.read().await;
        script
            .write(json!({"jsonrpc": "2.0", "method": "event/a", "params": {"n": 1}}))
            .await;
        script
            .write(json!({"jsonrpc": "2.0", "method": "event/b"}))
            .await;
        script
            .write(json!({"jsonrpc": "2.0", "id": request["id"], "result": null}))
            .await;
        assert_eq!(call.await.unwrap(), Ok(Value::Null));
        assert_eq!(
            incoming.recv().await,
            Some(Incoming::Notification {
                method: "event/a".into(),
                params: json!({"n": 1})
            })
        );
        assert_eq!(
            incoming.recv().await,
            Some(Incoming::Notification {
                method: "event/b".into(),
                params: Value::Null
            })
        );
    }

    #[tokio::test]
    async fn the_other_sides_requests_are_delivered_and_answered() {
        let (peer, mut incoming, mut script) = connect();
        script
            .write(json!({"jsonrpc": "2.0", "id": 7, "method": "request/permission", "params": {"toolName": "Bash"}}))
            .await;
        script
            .write(json!({"jsonrpc": "2.0", "id": "x", "method": "request/question", "params": {}}))
            .await;
        let Some(Incoming::Request { id, method, params }) = incoming.recv().await else {
            panic!("a request");
        };
        assert_eq!(
            (id.clone(), method.as_str()),
            (json!(7), "request/permission")
        );
        assert_eq!(params, json!({"toolName": "Bash"}));
        peer.respond(id, json!({"allow": true}));
        assert_eq!(
            script.read().await,
            json!({"jsonrpc": "2.0", "id": 7, "result": {"allow": true}})
        );
        let Some(Incoming::Request { id, .. }) = incoming.recv().await else {
            panic!("a request");
        };
        peer.respond_error(id, -32800, "declined");
        assert_eq!(
            script.read().await,
            json!({"jsonrpc": "2.0", "id": "x", "error": {"code": -32800, "message": "declined"}})
        );
    }

    #[tokio::test]
    async fn closing_fails_pending_calls_once() {
        let (peer, mut incoming, mut script) = connect();
        let call = tokio::spawn({
            let peer = peer.clone();
            async move { peer.call("session/prompt", json!({}), None).await }
        });
        script.read().await;
        drop(script);
        assert_eq!(call.await.unwrap(), Err(RpcError::Closed));
        assert!(matches!(
            incoming.recv().await,
            Some(Incoming::Closed { .. })
        ));
        peer.close("again");
        assert!(peer.is_closed());
        peer.closed().await;
        assert_eq!(
            peer.call("x", Value::Null, None).await,
            Err(RpcError::Closed)
        );
        assert_eq!(incoming.recv().await, None);
    }

    #[tokio::test]
    async fn a_local_close_ends_the_connection() {
        let (peer, mut incoming, mut script) = connect();
        peer.close("stopped");
        assert_eq!(
            incoming.recv().await,
            Some(Incoming::Closed {
                reason: "stopped".into()
            })
        );
        assert_eq!(script.frames.next().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_call_times_out() {
        let (peer, _incoming, mut script) = connect();
        let result = peer
            .call("slow", Value::Null, Some(Duration::from_millis(20)))
            .await;
        assert_eq!(result, Err(RpcError::Timeout));
        let request = script.read().await;
        script
            .write(json!({"jsonrpc": "2.0", "id": request["id"], "result": 1}))
            .await;
        let call = tokio::spawn({
            let peer = peer.clone();
            async move { peer.call("next", Value::Null, None).await }
        });
        let request = script.read().await;
        script
            .write(json!({"jsonrpc": "2.0", "id": request["id"], "result": 2}))
            .await;
        assert_eq!(call.await.unwrap(), Ok(json!(2)));
    }
}
