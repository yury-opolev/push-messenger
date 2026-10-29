//! Test doubles for the Coda backend: a [`FakeLauncher`] whose every spawn is a
//! [`FakeCoda`] the test scripts over in-memory pipes.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use tokio::io::{DuplexStream, duplex};
use tokio::sync::{mpsc, oneshot};

use super::backend::{Launcher, Spawned};
use super::framing::{FrameReader, write_frame};
use crate::backend::PermissionMode;

/// The in-memory pipe size: large enough that scripted writes never block.
const PIPE: usize = 1 << 20;

type ExitSender = Arc<Mutex<Option<oneshot::Sender<String>>>>;

/// A [`Launcher`] that records each spawn and hands the test a [`FakeCoda`] for it.
pub struct FakeLauncher {
    spawns: Mutex<Vec<(PathBuf, PermissionMode)>>,
    failures: Mutex<usize>,
    fakes: mpsc::UnboundedSender<FakeCoda>,
}

impl FakeLauncher {
    /// The launcher and the receiver of its [`FakeCoda`]s, one per spawn.
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<FakeCoda>) {
        let (fakes, receiver) = mpsc::unbounded_channel();
        let launcher = Self {
            spawns: Mutex::new(Vec::new()),
            failures: Mutex::new(0),
            fakes,
        };
        (Arc::new(launcher), receiver)
    }

    /// Every spawn so far: `(folder, permission mode)`.
    pub fn spawns(&self) -> Vec<(PathBuf, PermissionMode)> {
        self.spawns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The next `count` spawns fail.
    pub fn fail_spawns(&self, count: usize) {
        *self.failures.lock().unwrap_or_else(PoisonError::into_inner) = count;
    }
}

#[async_trait::async_trait]
impl Launcher for FakeLauncher {
    async fn spawn(&self, folder: &Path, mode: PermissionMode) -> io::Result<Spawned> {
        let failing = {
            let mut left = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
            let failing = *left > 0;
            *left = left.saturating_sub(1);
            failing
        };
        if failing {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no coda here"));
        }
        self.spawns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((folder.to_owned(), mode));
        let (backend_writer, fake_reader) = duplex(PIPE);
        let (fake_writer, backend_reader) = duplex(PIPE);
        let (exit_sender, exited) = oneshot::channel();
        let exit: ExitSender = Arc::new(Mutex::new(Some(exit_sender)));
        let killed = Arc::new(AtomicBool::new(false));
        let fake = FakeCoda {
            folder: folder.to_owned(),
            mode,
            frames: FrameReader::new(fake_reader),
            writer: Some(fake_writer),
            exit: Arc::clone(&exit),
            killed: Arc::clone(&killed),
            answers: HashMap::new(),
            backlog: VecDeque::new(),
            next_id: 1000,
        };
        let _ = self.fakes.send(fake);
        Ok(Spawned {
            reader: Box::new(backend_reader),
            writer: Box::new(backend_writer),
            exited,
            kill: Box::new(move || {
                killed.store(true, Ordering::SeqCst);
                send_exit(&exit, "killed");
            }),
        })
    }
}

fn send_exit(exit: &ExitSender, status: &str) {
    let sender = exit.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(sender) = sender {
        let _ = sender.send(status.to_owned());
    }
}

/// A scripted `coda serve`: reads the backend's frames and writes responses,
/// notifications and requests of its own.
pub struct FakeCoda {
    /// The `--cwd` it was started with.
    pub folder: PathBuf,
    /// The `--permission-mode` it was started with.
    pub mode: PermissionMode,
    frames: FrameReader<DuplexStream>,
    writer: Option<DuplexStream>,
    exit: ExitSender,
    killed: Arc<AtomicBool>,
    answers: HashMap<String, Value>,
    /// Requests read while waiting for a response.
    backlog: VecDeque<Value>,
    next_id: u64,
}

impl FakeCoda {
    /// A successful `initialize` result for `session_id`, with state events.
    pub fn initialize_result(session_id: &str) -> Value {
        json!({
            "protocolVersion": "1",
            "sessionId": session_id,
            "serverInfo": "coda",
            "contractVersion": "2026-09-1",
            "engineInstanceId": "engine-1",
            "eventCursor": 0,
            "capabilities": { "state.turnEvents": { "supported": true } },
        })
    }

    /// From now on, every request for `method` is answered with `result` without
    /// reaching [`Self::request`].
    pub fn answer(&mut self, method: &str, result: Value) {
        self.answers.insert(method.to_owned(), result);
    }

    /// The backend's next frame, answering requests set up with [`Self::answer`];
    /// `None` once the backend closed its side.
    pub async fn frame(&mut self) -> Option<Value> {
        loop {
            let frame = self.frames.next().await.ok().flatten()?;
            let method = frame.get("method").and_then(Value::as_str);
            let answer = method.and_then(|method| self.answers.get(method)).cloned();
            match (frame.get("id"), answer) {
                (Some(id), Some(result)) => {
                    let id = id.clone();
                    self.respond(&id, result).await;
                }
                _ => return Some(frame),
            }
        }
    }

    /// The backend's next request: `(id, method, params)`. Panics when the backend
    /// closed its side or sent something else.
    pub async fn request(&mut self) -> (Value, String, Value) {
        let frame = match self.backlog.pop_front() {
            Some(frame) => frame,
            None => self
                .frame()
                .await
                .expect("the backend closed the connection"),
        };
        let method = frame
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("expected a request, got {frame}"))
            .to_owned();
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let params = frame.get("params").cloned().unwrap_or(Value::Null);
        (id, method, params)
    }

    /// The next request, which must be for `method`: `(id, params)`.
    pub async fn expect(&mut self, method: &str) -> (Value, Value) {
        let (id, actual, params) = self.request().await;
        assert_eq!(actual, method, "unexpected request");
        (id, params)
    }

    /// Answers `initialize` with [`Self::initialize_result`]; returns its params.
    pub async fn handshake(&mut self, session_id: &str) -> Value {
        let (id, params) = self.expect("initialize").await;
        self.respond(&id, Self::initialize_result(session_id)).await;
        params
    }

    pub async fn respond(&mut self, id: &Value, result: Value) {
        self.write(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
            .await;
    }

    pub async fn respond_error(&mut self, id: &Value, code: i64, message: &str) {
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }))
        .await;
    }

    pub async fn notify(&mut self, method: &str, params: Value) {
        self.write(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await;
    }

    /// Sends a request of Coda's own (`request/permission`, …); returns its id.
    pub async fn send_request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = json!(self.next_id);
        self.write(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        id
    }

    /// The backend's response to request `id` (the whole frame). Requests read
    /// meanwhile wait for [`Self::request`].
    pub async fn response(&mut self, id: &Value) -> Value {
        loop {
            let frame = self
                .frame()
                .await
                .expect("the backend closed the connection");
            if frame.get("method").is_some() {
                self.backlog.push_back(frame);
            } else if frame.get("id") == Some(id) {
                return frame;
            }
        }
    }

    /// The backend killed this process.
    pub fn killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }

    /// The process exits with `status`: its pipes close.
    pub fn exit(self, status: &str) {
        send_exit(&self.exit, status);
    }

    async fn write(&mut self, value: Value) {
        if let Some(writer) = self.writer.as_mut() {
            if write_frame(writer, &value).await.is_err() {
                self.writer = None;
            }
        }
    }
}
