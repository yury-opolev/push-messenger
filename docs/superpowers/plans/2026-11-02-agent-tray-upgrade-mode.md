# Agent bridge tray app, control channel and upgrade mode — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A per-user local control channel on every `push-agent-bridge`, an upgrade mode
(pause/resume around a Coda upgrade that keeps Coda's state), and `push-agent-tray`, a Tauri
v2 tray app that supervises the bridge instances on the machine (Windows first, Linux ready).

**Architecture:** A new small crate `push-bridge-control` owns the wire protocol (NDJSON),
the endpoint naming (instance id → pipe name / socket path) and the per-user transport
(Windows named pipe with a DACL + peer SID check; Unix socket 0600 + peer uid check). The
bridge (`push-bridge`) serves it through a `ControlHub`; the core publishes states and
content-free events and executes `restart_backend`, `reconnect`, `shutdown`,
`pause_backend`, `resume_backend`. All testable tray logic lives in the pure-Rust crate
`push-tray-core` (in the workspace, MSRV 1.85, CI-tested on Linux); the Tauri shell
`apps/push-agent-tray` is its own Cargo workspace, excluded from the root, built by a
separate Windows CI job.

**Tech Stack:** Rust 2024 (MSRV 1.85: no let-chains), tokio (net/process), serde_json,
sha2, windows-sys 0.61 (Windows only), Tauri v2 (shell only).

**Request:** email "Change request: agent bridge tray app and upgrade mode (Windows first,
Linux later)" (2026-11-02). Coda side contract (being built in parallel in coda-cli):
`session/prepareRestart {drainTimeoutMs?, reason?}` → `{ok, restartFile, drained,
interruptedTasks}`; error `-32012` "restart pending" while draining;
`event/lifecycle {lifecycle:"draining"}`; `initialize {…, sessionId, restore?: true |
"suspended"}` → `{…, restored?: {schedules, messages, settings:[names], interruptedTasks,
steering}}`; `bypassPermissions` never restored (it may be named in `restored.settings`).
Method-not-found (`-32601`) on `prepareRestart` → fallback. Parse leniently (every field
optional; counts may be numbers or arrays — use the array length).

---

## Global rules for every task

- Work in the worktree `C:\Users\yurio\Documents\github\push-agent-tray-wt`, branch
  `feat/agent-tray`. Never touch `push-entra-1a164092` or `.github/copilot/`.
- MSRV 1.85 for every crate in the root workspace: **no let-chains** (`if let … && …`),
  no `Option::is_none_or` is fine (1.82), no newer std APIs. Check with
  `cargo +1.85 build -p <crate>` (the 1.85 toolchain is installed).
- Gates per task: `cargo fmt --all -- --check`, `cargo clippy -p <crates> --all-targets
  --locked -- -D warnings` (on Windows, which also compiles `cfg(windows)` code), the
  crate's tests. CI is Linux: `cfg(unix)` code must compile there too — keep it simple and
  obviously portable; `cargo check --target x86_64-unknown-linux-gnu` is not available, so
  be careful and mirror the patterns shown here.
- Workspace lints forbid `unsafe_code`. A crate that needs FFI overrides its lints exactly
  like `crates/push-agent-key/Cargo.toml` (`[lints.rust] unsafe_code = "deny"`,
  `unsafe_op_in_unsafe_fn = "deny"`, `[lints.clippy] all = { level = "warn", priority =
  -1 }`, `undocumented_unsafe_blocks = "warn"`) and puts `#![allow(unsafe_code)]` only on
  the FFI module, every `unsafe` block with a `// SAFETY:` note.
- **Never** put message bodies, previews, plans, answers, tool targets, tokens, keys or
  enrollment codes into control-channel events/status or logs. Only states, counts, names
  (agent display name, handle, backend name/version, folder, session id), and error codes.
- Commits: conventional, ending with
  `Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>`.
- New dependencies: only from crates.io with permissive licences (deny.toml). Add
  workspace deps to the root `[workspace.dependencies]` when used by 2+ crates.

## File map

| Path | Responsibility |
|---|---|
| `crates/push-bridge-control/` (new) | protocol types, instance id + endpoint naming, per-user transport (server + client), Windows ACL/SID, Unix 0600/uid |
| `crates/push-bridge/src/control.rs` (new) | `ControlHub`: serves connections, status snapshot (watch), events (broadcast), forwards actions to the bridge |
| `crates/push-bridge/src/core/report.rs` (new) | the core computes its `BridgeState` and publishes `state_changed` and the other events |
| `crates/push-bridge/src/core/upgrade.rs` (new) | pause / drain / release / watch / resume / announce |
| `crates/push-bridge/src/exe.rs` (+ `exe/windows.rs`) (new) | resolve Coda's executable, fingerprint (len, mtime, file version), missing/locked probe |
| `crates/push-bridge/src/backend.rs`, `coda/backend.rs` | `prepare_restart`, `StartOptions.restore`, `Started.restored`, new `ErrorKind`s |
| `crates/push-bridge/src/main.rs`, `cli.rs`, `config.rs`, `messenger.rs` | wiring, `pause`/`resume` commands, `"control": false`, reconnect + profile |
| `crates/push-tray-core/` (new) | icon state mapping, tooltip, restart policy, history, tray.json, autostart, Scheduled Task detection/switch-over, supervisor, menu model, icon pixels, open-path |
| `apps/push-agent-tray/` (new, own workspace) | Tauri v2 shell: tray icon, native menu, details window, notifications |
| `.github/workflows/ci.yml` | new `tray-windows` job |
| `docs/agent-bridge.md`, `CHANGELOG.md` | docs |

---

## Task 1: `push-bridge-control` — protocol and endpoint naming

**Files:** create `crates/push-bridge-control/{Cargo.toml,src/lib.rs,src/protocol.rs,src/endpoint.rs}`; modify root `Cargo.toml` (member + `push-bridge-control = { path = "crates/push-bridge-control" }` workspace dep).

Cargo.toml: `name = "push-bridge-control"`, `publish = false`, workspace package fields,
deps `serde`, `serde_json`, `sha2`, `thiserror`, `tokio = { workspace = true, features =
["io-util", "net", "rt", "sync", "time", "macros"] }`; lints as in global rules (the
transport in Task 2 needs unsafe on Windows; set the override now).

### protocol.rs (wire format, newline-delimited JSON, one object per line, UTF-8, max line 64 KiB)

```rust
pub const PROTOCOL: u32 = 1;
pub const MAX_LINE: usize = 64 * 1024;

/// A client's request line: {"id":1,"method":"status","params":{}}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request { pub id: u64, #[serde(flatten)] pub call: Call }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Call {
    Status,                        // params may be absent or {}
    RestartBackend,
    Reconnect,
    Shutdown,
    PauseBackend(PauseParams),
    ResumeBackend,
}
// Unit variants must accept `"params": {}` or missing params: implement Deserialize by
// hand via an intermediate {id, method: String, params: Option<Value>} if serde's
// adjacently-tagged form refuses one of them; unknown methods → Err "unknown_method".

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PauseParams {
    #[serde(default)] pub reason: Option<String>,          // capped to 100 chars by the server
    #[serde(default)] pub drain_timeout_ms: Option<u64>,   // default 600_000
}

/// Server → client lines: a response or an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerLine { Response(Response), Event(EventLine) }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub error: Option<ErrorBody>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody { pub code: String, pub message: String }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventLine { #[serde(flatten)] pub event: Event, pub at: i64 /* unix ms */ }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Hello { protocol: u32, bridge: String, pid: u32, instance: String },
    StateChanged { state: BridgeState, #[serde(default, skip_serializing_if = "Option::is_none")] detail: Option<String> },
    TurnStarted { source: String /* "prompt" | "engine" */ },
    TurnEnded { interrupted: bool },
    RequestOpened { kind: String /* "permission"|"question"|"plan"|"bypass" */, open: usize },
    RequestClosed { kind: String, outcome: String /* "answered"|"declined"|"expired" */, open: usize },
    Error { code: String, #[serde(default, skip_serializing_if = "Option::is_none")] method: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeState {
    Starting,      // process up, signing in / opening the conversation
    Idle,          // connected, backend running, nothing to do
    Working,       // a turn or slot command runs
    Waiting,       // a card is open: waiting for the owner
    Reconnecting,  // the messenger connection is down (SDK reconnecting) or re-signing in
    Restarting,    // the backend is down and restarts after the backoff
    Paused,        // upgrade mode: draining, released, or waiting for coda.exe
    Stopping,
}

/// The `status` result. Every field is content-free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub protocol: u32,
    pub instance: String,
    pub pid: u32,
    pub bridge_version: String,
    pub state: Option<BridgeState>,     // None never on the wire once started; Default for tests
    pub detail: Option<String>,
    pub agent: Option<AgentInfo>,       // display name + handle once signed in
    pub connection: Option<String>,     // "connected" | "connecting" | "offline" | …
    pub folder: Option<String>,
    pub session: Option<String>,
    pub backend: Option<BackendInfo>,
    pub open_requests: usize,
    pub held_texts: usize,              // texts held during a pause (a count only)
    pub pause: Option<PauseInfo>,
    pub last_error: Option<LastError>,
    pub started_at: i64,
}
#[derive(…)] #[serde(rename_all = "camelCase")] pub struct AgentInfo { pub display_name: String, pub handle: String }
#[derive(…)] #[serde(rename_all = "camelCase")] pub struct BackendInfo { pub name: String, pub version: Option<String>, pub running: bool, pub running_tasks: usize }
#[derive(…)] #[serde(rename_all = "camelCase")] pub struct PauseInfo { pub phase: String /* "draining"|"released"|"waiting_for_file"|"resuming" */, pub reason: Option<String>, pub since: i64 }
#[derive(…)] #[serde(rename_all = "camelCase")] pub struct LastError { pub code: String, pub at: i64 }
```

Plus helpers: `pub fn now_ms() -> i64`; `pub fn encode<T: Serialize>(v: &T) -> String`
(JSON + `\n`); `pub fn parse_request(line: &str) -> Result<Request, ErrorBody>` (errors
`bad_request` / `unknown_method`, the id is echoed when readable — return
`(Option<u64>, ErrorBody)` in the error case so the server can still answer).

### endpoint.rs

```rust
/// 16 lowercase hex chars: the first 8 bytes of SHA-256 over the normalized folder.
/// Normalized = std::fs::canonicalize (else std::path::absolute, else as given) as a
/// string; on Windows strip a leading `\\?\` (and `\\?\UNC\` → `\\`), use `\` and
/// lowercase; on Unix no case change; a trailing separator is removed.
pub fn instance_id(dir: &Path) -> String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// `\\.\pipe\push-agent-bridge-<id>`
    Pipe(String),
    /// `$XDG_RUNTIME_DIR/push-agent-bridge/<id>.sock`
    Socket(PathBuf),
}
/// Windows: always a pipe. Unix: `None` when XDG_RUNTIME_DIR is unset/empty/relative
/// (the control channel is then off; documented). Takes the env value as a parameter for
/// tests: `endpoint_for(id, runtime_dir: Option<&OsStr>)`; `endpoint(id)` reads the env.
pub fn endpoint(id: &str) -> Option<Endpoint>;
pub fn endpoint_for(id: &str, runtime_dir: Option<&std::ffi::OsStr>) -> Option<Endpoint>;
```

- [ ] Tests (in each file's `mod tests`): every `Call` round-trips through JSON with the
  exact spellings (`"method":"pause_backend","params":{"reason":"x","drainTimeoutMs":5}`,
  `{"id":2,"method":"status"}` and `{"id":2,"method":"status","params":{}}` both parse);
  unknown method → `unknown_method` with the id; garbage → `bad_request`; each `Event`
  serializes as documented (snapshot strings, e.g.
  `{"event":"state_changed","state":"waiting","detail":"approval","at":5}`);
  `ServerLine` distinguishes responses from events; `instance_id` is stable, 16 hex,
  differs for two folders, ignores a trailing separator, and (Windows) is case-insensitive;
  `endpoint_for` gives `\\.\pipe\push-agent-bridge-<id>` on Windows and
  `<rt>/push-agent-bridge/<id>.sock` on Unix, `None` for a missing/relative runtime dir.
- [ ] Run `cargo test -p push-bridge-control`; clippy; `cargo +1.85 build -p push-bridge-control`.
- [ ] Commit `feat(bridge-control): wire protocol and instance endpoints`.

## Task 2: `push-bridge-control` — per-user transport

**Files:** create `src/transport.rs`, `src/transport/windows.rs` (`#![allow(unsafe_code)]`),
`src/transport/unix.rs`; Cargo.toml target dep
`[target.'cfg(windows)'.dependencies] windows-sys = { version = "0.61", features = ["Win32_Foundation", "Win32_Security", "Win32_Security_Authorization", "Win32_System_Pipes", "Win32_System_Threading", "Win32_Storage_FileSystem"] }`
(add more features only as the compiler demands).

Public API (same on both platforms):

```rust
/// A connected peer stream (server or client side).
pub type Stream = Box<dyn AsyncReadWrite>;   // trait AsyncReadWrite: AsyncRead + AsyncWrite + Send + Unpin {}
pub struct Listener { … }
impl Listener {
    /// Binds the endpoint for the current user only. Windows: first pipe instance
    /// (FILE_FLAG_FIRST_PIPE_INSTANCE → an existing pipe of that name, i.e. another
    /// bridge on the same folder or a squatter, is `ErrorKind::AddrInUse`), remote clients
    /// rejected, DACL `D:P(A;;GA;;;<user SID>)(A;;GA;;;SY)`. Unix: creates the parent dir
    /// 0700 (refuses an existing dir not owned by us or with group/other bits → error),
    /// removes a stale socket file only when connecting to it fails (a live one →
    /// AddrInUse), binds, sets the file mode 0600.
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self>;
    /// The next client that passed the peer check (Windows: GetNamedPipeClientProcessId
    /// → OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) → OpenProcessToken → TokenUser SID
    /// EqualSid to ours or SYSTEM S-1-5-18; Unix: peer_cred().uid() == the uid owning our
    /// runtime dir via MetadataExt). Refused peers are dropped and logged by the caller via
    /// the returned `Refused` count; `accept` keeps waiting.
    pub async fn accept(&mut self) -> io::Result<Stream>;
}
impl Drop for Listener { /* Unix: remove the socket file */ }
/// Connects as a client; verifies the server too (Windows: GetNamedPipeServerProcessId →
/// same SID check; Unix: peer uid). Not found → io::ErrorKind::NotFound.
pub async fn connect(endpoint: &Endpoint) -> io::Result<Stream>;
/// Windows only, pure: the SDDL for a user SID string.
#[cfg(windows)] pub fn pipe_sddl(user_sid: &str) -> String;
/// Windows: the DACL of an existing pipe as SDDL (GetSecurityInfo + ConvertSecurity-
/// DescriptorToStringSecurityDescriptorW with DACL_SECURITY_INFORMATION) — for tests and
/// diagnostics. And `current_user_sid() -> io::Result<String>`.
```

Windows implementation notes: use `tokio::net::windows::named_pipe::{ServerOptions,
ClientOptions}`; `ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true)
.create_with_security_attributes_raw(name, &mut sa as *mut SECURITY_ATTRIBUTES as *mut c_void)`
for the first instance and `.first_pipe_instance(false)` for the next ones (create the next
instance right after a client connects, before the peer check, so the name never
disappears). Keep the PSECURITY_DESCRIPTOR from
`ConvertStringSecurityDescriptorToSecurityDescriptorW` alive for the listener's life,
`LocalFree` it in `Drop`. Close every HANDLE you open (`CloseHandle`). Client: retry
`ClientOptions::new().open` on `ERROR_PIPE_BUSY` (231) up to 50 times with 20 ms sleeps.

Unix notes: `tokio::net::{UnixListener, UnixStream}`, `std::os::unix::fs::{PermissionsExt,
MetadataExt, DirBuilderExt}`; `peer_cred()` on `UnixStream` is safe.

- [ ] Tests in `crates/push-bridge-control/tests/transport.rs` (use a unique id per test,
  e.g. `format!("test-{}-{}", std::process::id(), n)`; on Unix set the endpoint under a
  temp folder inside `std::env::temp_dir()`/`target` — NOT a hard-coded `/tmp` path —
  by calling `endpoint_for(id, Some(dir))`):
  - a line written by the client arrives at the server and back (echo);
  - binding the same endpoint twice → `AddrInUse`;
  - `#[cfg(windows)]` the pipe's DACL (via the SDDL helper) contains exactly two ACEs:
    our SID and `SY`, protected (`D:P`), and `pipe_sddl` is exactly
    `D:P(A;;GA;;;S-1-5-21-1)(A;;GA;;;SY)` for `"S-1-5-21-1"`;
  - `#[cfg(windows)]` a client in this process passes the peer check (same SID);
  - `#[cfg(unix)]` the socket file mode is `0o600` and its dir `0o700`; a pre-existing
    stale socket file is replaced; a dir with mode 0o777 is refused;
  - connect to an endpoint nobody binds → `NotFound` (Unix: also `ConnectionRefused`
    mapped to NotFound).
- [ ] Gates + `cargo +1.85 build -p push-bridge-control`. Commit
  `feat(bridge-control): per-user named pipe and Unix socket transport`.

## Task 3: the bridge serves the control channel (status, restart_backend, reconnect, shutdown)

**Files:** `crates/push-bridge/Cargo.toml` (dep `push-bridge-control`), `src/lib.rs`
(`pub mod control;`), create `src/control.rs`, create `src/core/report.rs`, modify
`src/core.rs`, `src/core/turns.rs`, `src/core/requests.rs`, `src/core/conversation.rs`,
`src/core/control.rs`, `src/messenger.rs`, `src/config.rs`, `src/main.rs`,
`src/core/tests.rs`, `tests/bridge.rs`.

### config
`Config` gains `#[serde(default = "yes")] pub control: bool` (default `true`; `"control":
false` turns the channel off; the bridge behaves exactly as before). Test: absent → true;
`false` parses.

### messenger
Add to `trait Messenger`:
```rust
/// The agent's own display name and handle, for the control channel's status.
async fn profile(&self) -> Option<(String, String)> { None }
/// Drops the connection and signs in again with a fresh assertion, then connects.
async fn reconnect(&self) -> Result<(), SdkError> { Err(SdkError::Invalid("reconnect is not available".into())) }
```
`SdkMessenger`: `profile()` reads `client.profiles(vec![me])` (display_name, handle);
`reconnect()` calls an optional `Arc<dyn Fn() -> BoxFuture<'static, Result<(), SdkError>> + Send + Sync>`
set with `SdkMessenger::with_reconnect(f)` (use `std::pin::Pin<Box<dyn Future<…> + Send>>`,
no new deps). main.rs supplies: `client.sign_in_with_agent_key(signer, Some(&audience),
&device_name, Platform::Desktop).await` then `client.connect()`. `TestMessenger`: returns
`Some(("Coda on Devbox".into(), "coda-devbox".into()))` and records `Call::Reconnect`
(add the variant) and returns Ok (a `fail_next_reconnect` helper is optional).

### control.rs (ControlHub)

```rust
pub enum Action { RestartBackend, Reconnect, Shutdown, Pause(PauseParams), Resume }
pub struct ControlCall { pub action: Action, pub reply: oneshot::Sender<Result<serde_json::Value, ErrorBody>> }

#[derive(Clone)]
pub struct ControlHandle { status: Arc<watch::Sender<Status>>, events: broadcast::Sender<EventLine> }
impl ControlHandle {
    pub fn update(&self, change: impl FnOnce(&mut Status));      // send_if_modified
    pub fn emit(&self, event: Event);                             // stamps `at`
    pub fn status(&self) -> Status;
    /// A handle with no listener (for tests and `"control": false`): updates go nowhere.
    pub fn detached() -> Self;
    pub fn subscribe(&self) -> broadcast::Receiver<EventLine>;   // tests
}
pub struct ControlHub { handle: ControlHandle, calls: Arc<tokio::sync::Mutex<mpsc::Receiver<ControlCall>>>, sender: mpsc::Sender<ControlCall> }
impl ControlHub {
    pub fn new(instance: String) -> Self;  // Status { protocol, instance, pid, bridge_version: CARGO_PKG_VERSION, state: Some(Starting), started_at: now }
    pub fn handle(&self) -> ControlHandle;
    pub fn calls(&self) -> Arc<tokio::sync::Mutex<mpsc::Receiver<ControlCall>>>;
    /// Serves a bound listener until dropped: per connection, sends Hello, then a
    /// `state_changed` with the current state, then forwards every event and answers
    /// requests (status from the snapshot; the rest as ControlCall, answered when the
    /// bridge replies; 15 min cap for pause, 30 s for others → error "timeout"). Lines
    /// over MAX_LINE close the connection. At most 8 concurrent connections (more are
    /// closed at once). A lagging subscriber skips missed events (RecvError::Lagged).
    pub fn serve(&self, listener: Listener) -> tokio::task::JoinHandle<()>;
    /// Serves a single already-connected stream (used by `serve` and by tests with
    /// `tokio::io::duplex`).
    pub fn serve_stream<S: AsyncRead + AsyncWrite + Send + Unpin + 'static>(&self, stream: S) -> JoinHandle<()>;
}
```

### core
- `BridgeOptions` stays; `Bridge::new` unchanged; add `Bridge::with_control(self, handle:
  ControlHandle, calls: Arc<tokio::sync::Mutex<mpsc::Receiver<ControlCall>>>) -> Self`.
  Without it the core uses `ControlHandle::detached()` and a channel nobody sends to.
- In `run`, lock `calls` for the run's life and add a `select!` arm
  `Some(call) = calls.recv() => match core.on_control(call).await { Flow::Continue => {}, Flow::Stop => break Ok(()) }`.
- `report.rs`: `fn bridge_state(&self) -> (BridgeState, Option<String>)`:
  stopping → Stopping; pause.is_some() → Paused ("upgrading Coda" / phase); connection is
  Some and != Connected → Reconnecting; `restart_at.is_some() || running_since.is_none()` →
  Restarting; open card → Waiting (detail = the waiting label: "approval", "question",
  "plan approval"); turn or slot_commands>0 → Working (no detail: tool labels can carry
  paths/commands); else Idle. `fn publish(&mut self)` emits `StateChanged` when the pair
  changed and updates the snapshot (state, detail, folder, session, backend {name,
  version, running, running_tasks}, connection, open_requests, held_texts, pause,
  last_error). Call `publish()` at the end of `update_activity` and wherever state changes
  without an activity update (adopt, exited, schedule_restart, connection_changed, pause
  transitions). `running_tasks`: count TaskStarted minus TaskCompleted ids (a `HashSet`),
  cleared when the backend exits or restarts.
- Events: `turn_started {source}` in `begin_turn`, `turn_ended {interrupted}` in
  `end_turn`; `request_opened {kind, open}` in `post_card` after push; `request_closed
  {kind, outcome, open}` in `settle` ("answered"), `expire_requests` ("expired"),
  `close_backend_requests`/`cancel_bypass_request`/`send_failed`/`close` ("declined");
  `error {code, method}` in `backend_failed` and `start_or_retry` failures (also sets
  `last_error`). kind strings from `CardKind`: permission | question | plan | bypass.
- `on_control`:
  - `RestartBackend` → `restart_by_owner()`: like `exited()` but the note is
    "(Coda is restarting: the owner asked)" (use `self.name`), requests declined with
    "Declined: {name} restarted.", `backend.stop().await`, then start at once (no backoff,
    `backoff` untouched) with `start_or_retry(...)` where the success reply is
    `restarted_note("restarted by the owner")` → "Coda restarted by the owner." + the lost
    list (refactor `restarted_note(&self, how: &str)`; crash path passes "restarted").
    The messenger is untouched. Reply `{"restarted": true}` (or the start error).
  - `Reconnect` → `messenger.reconnect().await`; Ok → reply `{"reconnected": true}`, set
    `connection = None` so the next Connected counts as a reconnect (activity resent,
    pending fetched); Err → `ErrorBody{code: error.code(), …}`. The backend is untouched.
  - `Shutdown` → reply `{"stopping": true}` then `Flow::Stop` (the run loop ends Ok).
  - `Pause`/`Resume` → Task 6 (until then reply error `unsupported`).
- `close()`: if any open request, decline them all (backend responders via `backend.stop`
  as today; bypass card closed) and reply **"Declined: the bridge was stopped."** once;
  publish Stopping first. Applies to Ctrl-C too.

### main.rs
- After `load_state`, when `config.control`: `let hub = ControlHub::new(instance_id(dir))`;
  `endpoint(&id)` → `Listener::bind` → `hub.serve(listener)`; a bind error logs
  `control_unavailable` with the io error kind and the bridge continues without it.
- Between bridge runs (sign-in wait, sign-in) the main loop also `select!`s on the calls
  receiver: `Shutdown` → reply and stop (exit 0); `Reconnect` → reply and skip the
  remaining `SIGN_IN_INTERVAL` wait; others → `ErrorBody{code:"unavailable", message:"the
  bridge is signing in"}`. Status is always answered by the hub from the snapshot.
- Snapshot updates in main: `agent` from `messenger.profile()` after sign-in, `connection`.
- Exit codes unchanged; a `shutdown` request ends `run` with 0.

### Tests (acceptance: bridge control channel)
In `src/core/tests.rs` (Harness gets a `ControlHub`, `with_control`, and a helper
`async fn call(&self, action) -> Result<Value, ErrorBody>`; events via `subscribe()`):
- [ ] `status_reports_state_folder_session_backend_and_counts` — after start: state Idle,
  folder = work, session = stored id, backend name "Test" running, open 0; open a card →
  Waiting with detail; held 0.
- [ ] `restart_backend_keeps_the_messenger_session` — a second `start` call, no
  `Call::Reconnect`, reply "Coda restarted by the owner." (TestBackend name), the
  conversation is the same, events show Restarting → Idle.
- [ ] `reconnect_signs_in_again_and_keeps_the_backend` — `Call::Reconnect` recorded once,
  `start` count unchanged; after injecting Connected the activity is resent.
- [ ] `shutdown_declines_open_requests_and_stops_ok` — open a permission card, call
  Shutdown → run returns `Ok(())`, reply "Declined: the bridge was stopped.", the
  responder sees a decline, events: request_closed(declined), state Stopping.
- [ ] `events_and_status_carry_no_message_content` — run a turn with a secret prompt, secret
  text deltas, a permission with a secret preview, a question, an error event with a secret
  message, a background task with a secret description; serialize every event received and
  the final status; assert none contains the secret marker.
- [ ] `ControlHub` tests in `src/control.rs` with `tokio::io::duplex`: Hello first, then
  state_changed; status answered from snapshot without any bridge; an action is forwarded
  and its reply returned with the same id; a bad line → error `bad_request` and the
  connection stays; an over-long line closes the connection.
- [ ] `tests/bridge.rs`: one end-to-end test through a real `Listener` + `connect` (unique
  id; on Unix with a runtime dir under `std::env::temp_dir()`): status over the transport
  and `shutdown` ending `Bridge::run` with Ok.
- [ ] Gates; commit `feat(bridge): local control channel with status, restart_backend, reconnect and shutdown`.

## Task 4: backend contract for planned restarts

**Files:** `src/backend.rs`, `src/coda/backend.rs`, `src/coda/backend/tests.rs`,
`src/test_backend.rs`.

```rust
// backend.rs
pub enum ErrorKind { …, /// The backend has no such method (an older version).
    Unsupported, /// The backend is draining for a restart (Coda -32012).
    RestartPending }
pub struct StartOptions { …, /// Restore the state the backend saved before a planned restart.
    pub restore: bool }
pub struct Started { …, pub restored: Option<Restored> }
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Restored { pub schedules: usize, pub messages: usize, pub settings: Vec<String>, pub interrupted_tasks: usize, pub steering: usize }
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prepared { pub drained: bool, pub interrupted_tasks: usize }
// trait Backend:
/// Saves the backend's state for a restart after letting running work finish (up to
/// `drain`). Default: Unsupported.
async fn prepare_restart(&self, drain: Duration, reason: Option<&str>) -> Result<Prepared, BackendError> { Err(unsupported("session/prepareRestart")) }
/// The program file the backend runs, for upgrade detection; None when unknown.
fn executable(&self) -> Option<PathBuf> { None }
```
Every existing `StartOptions { … }` literal gets `restore: false`; every `Started { … }`
gets `restored: None`.

Coda:
- `backend_error`: `-32601` → `Unsupported`; `-32012` → `RestartPending` (checked before
  the "busy" text rule).
- `prepare_restart`: `session/prepareRestart {"drainTimeoutMs": ms, "reason"?: r}` with
  timeout `drain + 60 s`; result lenient: `drained` (bool, default true when absent),
  `interruptedTasks` (number, or array length, default 0); `ok:false` → Refused.
- `start`: when `options.restore`, add `"restore": true` to initialize; parse
  `result.restored` leniently (`count(v, key)`: number → n, array → len, missing → 0;
  `settings`: array of strings, or object keys); `restored: None` when absent.
- `executable()`: `Some(config.exe)` (via `ProcessLauncher`; `with_launcher` → None
  unless set with `CodaBackend::with_executable(path)` for tests).
- `event/lifecycle` is ignored (logged at debug).
- TestBackend: `next_prepare(Result<Prepared, BackendError>)` (default
  `Err(error("session/prepareRestart", ErrorKind::Unsupported))`), records
  `prepare drain=<ms>` calls, records `restore=true` in the start call text only when set
  (keep the old text otherwise so existing assertions hold), `next_start` already scripts
  `Started` (tests pass `restored`), `set_executable(Option<PathBuf>)`.

- [ ] FakeCoda tests: prepareRestart params and result parsing (numbers and arrays);
  `-32601` → Unsupported; `-32012` on prompt → RestartPending; initialize with restore
  sends `"restore": true` and parses `restored` (incl. `settings` naming
  bypassPermissions); without restore the params have no `restore` key.
- [ ] Gates; commit `feat(bridge): Coda planned-restart contract (prepareRestart, restore)`.

## Task 5: Coda's executable — resolve, fingerprint, probe

**Files:** create `src/exe.rs` and `src/exe/windows.rs`; `Cargo.toml` (lints override like
push-agent-key; `[target.'cfg(windows)'.dependencies] windows-sys = { version = "0.61",
features = ["Win32_Foundation", "Win32_Storage_FileSystem"] }`); `src/lib.rs`.

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint { pub path: PathBuf, pub len: u64, pub modified: Option<SystemTime>, pub version: Option<String> }
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe { Missing, Locked, Usable(Fingerprint) }
/// `exe` as configured: a path with a separator is used as it is; a bare name is looked
/// up in PATH (Windows: also with each PATHEXT extension when it has none). None: not found.
pub fn resolve(exe: &Path) -> Option<PathBuf>;
/// Missing: resolve fails or NotFound; Locked: open for reading fails with another error
/// (Windows sharing violation 32 / lock 33 / access denied 5) or the length is 0;
/// Usable: opened, with the fingerprint. `version` = Windows file version "a.b.c" (VS_FIXEDFILEINFO,
/// trailing ".0" dropped), None elsewhere.
pub fn probe(exe: &Path) -> Probe;
```
`exe/windows.rs`: `pub fn file_version(path: &Path) -> Option<String>` with
`GetFileVersionInfoSizeW` / `GetFileVersionInfoW` / `VerQueryValueW(L"\\")`.

- [ ] Tests: resolve with a separator; resolve a bare name through a PATH you construct (pass
  PATH as a parameter: `resolve_in(exe, path_var: Option<&OsStr>, pathext: Option<&OsStr>)`);
  probe Missing for a missing file; Usable with len/modified for a file you write;
  fingerprint changes after rewriting the file with different content;
  `#[cfg(windows)]` Locked while another handle holds it open with `share_mode(0)`
  (`std::os::windows::fs::OpenOptionsExt`); `#[cfg(windows)]` `file_version` of
  `C:\Windows\System32\notepad.exe` is Some and has a dot (skip if the file is absent).
- [ ] Gates; commit `feat(bridge): probe Coda's executable for upgrades`.

## Task 6: upgrade mode in the core (pause flow, watch, resume, announce) + CLI

**Files:** create `src/core/upgrade.rs`; modify `src/core.rs`, `src/core/turns.rs`,
`src/core/conversation.rs`, `src/core/control.rs`, `src/core/report.rs`,
`src/core/tests.rs`, `src/cli.rs`, `src/main.rs`, `tests/bridge.rs`.

State:
```rust
pub(super) struct Pause {
    phase: Phase, reason: Option<String>, since: Instant, since_ms: i64,
    deadline: Instant,                 // drain deadline
    prepared: Option<Prepared>,        // Some when prepareRestart succeeded
    supported: Option<bool>,           // None until prepareRestart answered
    before: Option<Fingerprint>,       // the exe before the release (None: unknown)
    version_before: Option<String>,    // Started.version or file version
    bypass_before: bool,
    waiters: Vec<oneshot::Sender<Result<Value, ErrorBody>>>, // pause_backend callers
    told_owner: bool,                  // "Coda is being upgraded…" sent once
    stable: Option<Fingerprint>,       // last probe, for the "unchanged over two probes" rule
    killed: bool,                      // entered after an unplanned exit (no restore)
}
enum Phase { Draining, Released, Resuming }
```
Flow (spec steps 1–5):
1. `pause_backend {reason, drainTimeoutMs=600000}`: already paused → reply at once with the
   current phase (idempotent; a Released pause answers `{"state":"paused","phase":"released"}`);
   else create `Pause`, publish Paused, activity (when no card is open)
   `Working` with label **"Paused: upgrading Coda"**. From now on owner text that would go
   to the agent is appended to `queued` (commands: `/status`, `/help` work; any other
   command replies "Coda is paused for an upgrade; try again when it is back." and card
   answers by text/card still settle). The first held text gets the reply **"Coda is being
   upgraded; your messages will be sent when it is back."** (once per pause).
   `send_queued` does nothing while paused. A prompt that fails with `RestartPending` is
   pushed back into `queued`.
2. Spawn `backend.prepare_restart(drain, reason)` (result as `Internal::Prepared`). While it
   runs, turns, cards and answers flow as normal. `Unsupported` → fallback: wait until
   `turn.is_none() && slot_commands == 0` or the deadline (checked in `on_tick` and after
   `end_turn`/`commanded`). Other errors → the pause fails: reply to waiters with the error,
   leave pause (held text is sent), owner hears "Pausing Coda failed: <method> failed (<code>)".
3. Release: remember `bypass_before = mode == BypassPermissions`, `version_before`
   (`self.version` or the probe's file version), `before = probe(exe)` fingerprint; end a
   still-running turn with the note "(Coda paused for an upgrade)"; decline open backend
   requests "Declined: Coda is being upgraded."; `backend.stop().await` (retires: no
   Exited); `running_since = None`; phase Released; answer waiters
   `{"state":"paused","prepared":bool,"drained":bool,"interruptedTasks":n}`; publish.
4. Watch: every 2 s (count ticks) while Released: `probe(exe)`; resume when `Usable(fp)` and
   `fp != before` (path/len/mtime/version) and the same fp was seen on the previous probe
   (stable). `resume_backend` request: resume now if the probe is Usable (changed or not),
   else reply error `exe_unavailable` ("coda.exe is missing or in use"). Not paused →
   error `not_paused`. While Draining, `resume_backend` cancels the pause (leave pause,
   send held text, reply `{"resumed": true, "cancelled": true}`) — Coda keeps running;
   if prepareRestart already succeeded the next start uses restore anyway (keep a
   `restore_next` flag).
5. Resume: phase Resuming; start the stored session with `restore = prepared.is_some() &&
   !killed`, mode = `default_mode` if `bypass_before` else `mode`. Failure → the normal
   `start_or_retry` backoff path (pause cleared). Success → announcement:
   - with `restored`: **"Coda upgraded 0.1.176 → 0.1.177: 2 schedules, 1 message and the
     model/goal restored; 1 interrupted task reported to Coda."** Rules: head is
     "{name} upgraded {a} → {b}" when both versions known and differ, "{name} upgraded to
     {b}" when only b, "{name} restarted" otherwise; items: "N schedule(s)", "N
     message(s)", settings except `bypassPermissions`/`permissionMode` joined with "/"
     preceded by "the " (e.g. "the model/goal"); joined "a, b and c" + " restored"; when
     nothing → "nothing to restore"; then "; N interrupted task(s) reported to {name}" when
     > 0; ends with ".".
   - without `restored` (fallback/killed): head + ". " + the existing lost list
     ("Undelivered background messages and in-memory schedules were lost.").
   - `bypass_before` → after the announcement post `cards::bypass_card(&self.name)` again
     (a new confirmation), with the reply "Approvals were off before the upgrade; confirm to
     turn them off again." before it.
   Then clear pause, publish, `send_queued()` (held texts as one prompt).
6. Unplanned exit (`exited()`) and failed start (`start_or_retry` Err): when
   `backend.executable()` is Some and `probe` is Missing or Locked → enter `Pause { killed:
   true, phase: Released, reason: "Coda's program is missing or in use" }` instead of
   `schedule_restart()` (no backoff spent); reply once "(Coda stopped and its program is
   missing or in use; waiting for it to come back)". The watch resumes when Usable and
   stable (no "changed" requirement when `before` is None).

Version for the "after" side: `Started.version` if it changed from before, else the new
file version from the probe.

CLI (`cli.rs`): commands `pause [--reason <text>] [--drain-timeout <seconds>]` and
`resume`. main: compute `instance_id(&dir)` → `endpoint` → `connect`; read Hello; send
the request; wait for the response (pause: up to drain + 120 s; resume: 30 s); print
"Coda is paused; its program is released." / "Coda is resuming." or the error; exit 0 /
1 (not running: "The bridge in <dir> is not running (no control channel)." exit 1).
USAGE and the docs' command table updated.

### Tests (acceptance: pause flow), in `src/core/tests.rs` with paused time and a real temp exe file (`TestDir`) set through `backend.set_executable`:
- [ ] `a_pause_during_a_turn_waits_for_it_and_holds_owner_text` — turn running, pause →
  prepare called, activity Working "Paused: upgrading Coda" not yet sent while a card is
  open…; owner text → held + reply "Coda is being upgraded; …" once (second text no second
  reply); the turn ends normally (no "Declined"), prepare returns → stop called → waiter
  answered, nothing declined.
- [ ] `after_the_file_changes_it_resumes_with_restore_and_announces` — rewrite the exe file
  (different content), advance 4 s → `start … restore=true`, announcement exactly
  "Test upgraded 1.0 → 1.1: 2 schedules, 1 message and the model/goal restored; 1
  interrupted task reported to Test." (script versions/restored), held text prompted as one
  "first\n\nsecond".
- [ ] `bypass_needs_a_new_confirmation_after_an_upgrade` — mode bypass before; resume starts
  with mode default and posts a bypass card; settings naming bypassPermissions is not
  mentioned.
- [ ] `the_fallback_works_without_prepare_restart` — prepare → Unsupported; turn running →
  waits for turn end; then stop; resume after file change with restore=false; announcement
  lists the lost items.
- [ ] `a_killed_coda_with_a_locked_exe_pauses_instead_of_restarting` — `#[cfg(windows)]`
  lock the file (share_mode 0), emit Exited → no `start` within 5 min of paused time, state
  Paused; unlock → start again (restore=false). Plus a portable variant with a *missing*
  exe (delete the file; recreate it).
- [ ] `resume_without_pause_is_an_error` and `pause_is_idempotent`.
- [ ] CLI parse tests for `pause --reason x --drain-timeout 30`, `resume`, mistakes.
- [ ] Gates; commit `feat(bridge): upgrade mode — pause, release, watch and restore Coda`.

## Task 7: `push-tray-core` — states, icons, policy, tray.json, autostart, Scheduled Tasks

**Files:** create `crates/push-tray-core/{Cargo.toml, src/lib.rs, src/state.rs, src/icon.rs,
src/policy.rs, src/history.rs, src/tray_config.rs, src/paths.rs, src/autostart.rs,
src/schedtask.rs, src/open.rs}`; root `Cargo.toml` member. Deps: `push-bridge-control`,
`serde`, `serde_json`, `thiserror`, `tokio` (process, time, sync, rt, macros, io-util),
`tracing`. Workspace lints (no unsafe here: use `reg.exe`, `powershell.exe` and
`explorer.exe`/`xdg-open` as child processes with `CREATE_NO_WINDOW` via
`std::os::windows::process::CommandExt`).

- `state.rs`:
  ```rust
  pub enum AgentState { Idle, Working, Waiting(Option<String>), Starting, Reconnecting, Restarting, Paused, Stopping, Stopped, Failed { code: Option<i32>, retry_in: Option<Duration> }, GaveUp { code: Option<i32> }, NotSetUp, External /* run by a Scheduled Task, reachable */ }
  pub enum IconState { Normal, Busy, Waiting, Paused, Disconnected, Error }  // derive Ord in this order of severity (Error worst)
  pub fn icon_for(state: &AgentState) -> IconState;  // Idle→Normal, Working→Busy, Waiting→Waiting, Paused→Paused, Starting/Reconnecting/Restarting/Stopping→Disconnected, Stopped/Failed/GaveUp/NotSetUp→Error
  pub fn worst(states: impl IntoIterator<Item = IconState>) -> IconState; // max; empty → Normal
  pub fn label(state: &AgentState) -> String; // "connected, idle", "working", "waiting for you (approval)", "paused for an upgrade", "disconnected, reconnecting", "Coda restarting", "stopped", "failed (exit 1), retrying in 4 s", "stopped after 5 failures (exit 1)", "Not set up: run keygen and enroll", "starting", "stopping"
  pub fn from_bridge(state: BridgeState, detail: Option<String>) -> AgentState;
  pub fn tooltip(agents: &[(String, AgentState)]) -> String; // lines "Coda on Devbox: waiting for you (approval)"; first line "push-agent-tray" when empty ("No agents"); capped at 127 chars (Windows NOTIFYICONDATA limit) with "…"
  ```
- `icon.rs`: `pub fn rgba(state: IconState, size: u32) -> Vec<u8>` — a filled circle with
  a 1 px darker ring in the state colour (Normal green #2e9d4f, Busy blue #2f6fdf, Waiting
  amber #e8a317, Paused purple #8a5cd1, Disconnected grey #8c8c8c, Error red #d13438),
  transparent outside. Tests: length = size²·4, centre pixel colour, corner transparent,
  six colours distinct.
- `policy.rs` (supervision): 
  ```rust
  pub struct RestartPolicy { backoff_base: Duration (1 s), cap: Duration (60 s), window: Duration (10 min), max_exits: usize (5), reset_after: Duration (60 s), exits: VecDeque<Instant>, next: Duration }
  pub enum Decision { Restart(Duration), GiveUp, NotSetUp, Stopped }
  impl RestartPolicy { pub fn new() -> Self; pub fn on_exit(&mut self, code: Option<i32>, requested_stop: bool, ran_for: Duration, now: Instant) -> Decision; pub fn reset(&mut self) }
  ```
  Rules: `requested_stop` → Stopped (whatever the code). code 2 → NotSetUp (never
  retried). Otherwise record the exit; if ≥ 5 exits within the last 10 min → GiveUp; else
  `ran_for >= reset_after` resets the backoff; Restart(next) and double up to 60 s.
  Tests: 1,2,4,8,16… capped at 60; 5 quick exits → GiveUp on the 5th; exits spread over
  > 10 min never give up; exit 2 → NotSetUp even first time; requested stop → Stopped;
  long run resets backoff.
- `history.rs`: `History` ring of the last 50 `(i64 at_ms, AgentState label String)`;
  test the cap and order.
- `paths.rs`: `tray_json_path()` (Windows `%LOCALAPPDATA%\push-messenger\tray.json`; else
  `$XDG_CONFIG_HOME/push-messenger/tray.json`, default `~/.config/…`), `default_bridge_dir()`
  (same rule as `push_bridge::config::default_dir`: Windows `%LOCALAPPDATA%\push-messenger\coda-bridge`,
  else `$XDG_STATE_HOME/push-messenger/coda-bridge` / `~/.local/state/…`), each with a
  `_from(env lookup fn)` variant for tests. Do not depend on push-bridge.
- `tray_config.rs`:
  ```rust
  #[serde(deny_unknown_fields)] pub struct TrayConfig { #[serde(default = "one")] pub version: u32, #[serde(default)] pub instances: Vec<Instance> }
  #[serde(deny_unknown_fields)] pub struct Instance { pub dir: PathBuf, #[serde(default = "yes")] pub autostart: bool }
  impl TrayConfig {
      pub fn load(path) -> Result<Self, TrayConfigError>;   // missing file → default; BOM ok; corrupt → error (never overwritten silently: the tray shows the error)
      pub fn save(&self, path) -> Result<(), TrayConfigError>; // create parent, write tmp + rename
      pub fn add(&mut self, dir: &Path) -> Result<bool, TrayConfigError>; // requires dir/config.json; dedupe by push_bridge_control::instance_id; returns false when present
      pub fn remove(&mut self, dir: &Path) -> bool;
      pub fn ensure_default(&mut self, default_dir: &Path) -> bool; // adds it (autostart true) when it holds config.json and is absent
      pub fn set_autostart(&mut self, dir: &Path, on: bool) -> bool;
  }
  ```
  Tests: round trip; missing → empty; BOM; unknown field refused; add requires config.json;
  dedupe (trailing separator, Windows case); ensure_default adds once and only with
  config.json; atomic save leaves no `.tmp`.
- `autostart.rs`: trait `Autostart { fn enabled(&self) -> io::Result<bool>; fn set(&self, on: bool) -> io::Result<()>; }`;
  `RunKey { exe: PathBuf }` (Windows: `reg.exe query/add/delete HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v push-agent-tray /t REG_SZ /d "\"<exe>\" --autostart" /f`);
  `XdgAutostart { dir: PathBuf, exe: PathBuf }` writing
  `<XDG_CONFIG_HOME|~/.config>/autostart/push-agent-tray.desktop`
  (`[Desktop Entry]\nType=Application\nName=push-agent-tray\nExec="<exe>" --autostart\nX-GNOME-Autostart-enabled=true\n`).
  Pure helpers tested: `run_key_value(exe)`; XdgAutostart set/enabled/unset in a temp dir.
- `schedtask.rs`:
  ```rust
  pub struct TaskInfo { pub name: String, pub execute: String, pub arguments: String, pub state: String }
  /// From `Get-ScheduledTask | … | ConvertTo-Json` output (object or array; Actions
  /// flattened by the query below). Lenient.
  pub fn parse_tasks(json: &str) -> Vec<TaskInfo>;
  /// The bridge folder a task runs: the action runs push-agent-bridge(.exe) directly or
  /// through conhost --headless "<exe>"; `--dir "<d>"` / `--dir=<d>` / none (→ default dir);
  /// only `run` tasks count. None for other tasks.
  pub fn bridge_dir(task: &TaskInfo, default_dir: &Path) -> Option<PathBuf>;
  pub struct TaskFor { pub task: String, pub dir: PathBuf }
  pub fn tasks_for_instances(tasks: &[TaskInfo], dirs: &[PathBuf], default_dir: &Path) -> Vec<TaskFor>; // matched by instance_id
  pub enum Offer { None, SwitchOver(Vec<TaskFor>) }
  /// Offer the switch-over when any configured instance is run by a task; never both:
  pub fn offer(found: &[TaskFor]) -> Offer;
  pub struct SwitchPlan { pub steps: Vec<Step> }
  pub enum Step { StopTask(String), UnregisterTask(String), EnableTrayAutostart, MarkAutostart(PathBuf), StartBridge(PathBuf) }
  /// The ordered steps: for each task stop it, unregister it; then enable the tray's
  /// autostart, mark each instance autostart and start it under the tray.
  pub fn plan_switch(found: &[TaskFor]) -> SwitchPlan;
  /// Runs Get-ScheduledTask through powershell.exe (no window) with
  /// `Get-ScheduledTask | ForEach-Object { foreach ($a in $_.Actions) { [pscustomobject]@{ name = $_.TaskName; execute = $a.Execute; arguments = $a.Arguments; state = [string]$_.State } } } | ConvertTo-Json -Compress`
  pub trait Scheduler { fn list(&self) -> io::Result<Vec<TaskInfo>>; fn stop(&self, name: &str) -> io::Result<()>; fn unregister(&self, name: &str) -> io::Result<()>; fn run(&self, name: &str) -> io::Result<()>; }
  pub struct PowerShellScheduler;   // Stop-ScheduledTask / Unregister-ScheduledTask -Confirm:$false / Start-ScheduledTask with -TaskName '<escaped>' (single quotes doubled)
  pub fn execute_switch(plan: &SwitchPlan, scheduler: &dyn Scheduler, autostart: &dyn Autostart, config: &mut TrayConfig, start: &mut dyn FnMut(&Path)) -> io::Result<()>; // stops at the first failure, before enabling autostart if a task could not be removed (never both)
  ```
  Tests: parse single object and array and garbage; bridge_dir for the conhost form from
  install.ps1 (`--headless "C:\Tools\push-agent-bridge.exe" --dir "C:\x\coda-bridge" run`),
  the direct form, `--dir=`, no `--dir` (default), a non-bridge task, a `status` task (not
  run) → None; matching by instance id; offer; plan order; `execute_switch` with fakes:
  order of calls, a failing unregister leaves autostart off and instances unmarked.
- `open.rs`: `pub fn open_path(path: &Path) -> io::Result<()>` (Windows `explorer.exe
  <path>`, else `xdg-open <path>`), no shell.

- [ ] Gates + `cargo +1.85 build -p push-tray-core`; commit `feat(tray-core): states, icons, restart policy, tray.json, autostart and Scheduled Tasks`.

## Task 8: `push-tray-core` — control client, supervisor, menu model

**Files:** create `src/client.rs`, `src/supervisor.rs`, `src/menu.rs`, tests.

- `client.rs`: `pub struct ControlClient` over `push_bridge_control::transport::connect`:
  `connect(dir) -> io::Result<(ControlClient, mpsc::Receiver<EventLine>)>` (reads Hello,
  spawns a reader task routing responses to pending oneshots by id and events to the
  channel); `async fn call(&self, call: Call, timeout: Duration) -> Result<Value, ErrorBody>`;
  `async fn status(&self) -> Result<Status, ErrorBody>`. Also generic `connect_stream(S)`
  for tests with `tokio::io::duplex` (a fake server in the test).
- `supervisor.rs`:
  ```rust
  pub trait Spawner: Send + Sync { fn spawn(&self, dir: &Path) -> io::Result<Box<dyn Child>>; }
  pub trait Child: Send { fn id(&self) -> Option<u32>; fn try_wait(&mut self) -> io::Result<Option<i32>>; fn kill(&mut self) -> io::Result<()>; }
  pub struct ProcessSpawner { pub bridge_exe: PathBuf } // `<exe> --dir <dir> run`, CREATE_NO_WINDOW on Windows, stdin/stdout/stderr null
  pub fn bridge_exe_next_to(tray_exe: &Path) -> PathBuf; // push-agent-bridge(.exe) in the same folder
  pub struct InstanceView { pub dir: PathBuf, pub id: String, pub name: String /* display name (handle) or the folder name */, pub state: AgentState, pub started_by_tray: bool, pub task: Option<String>, pub status: Option<Status>, pub history: History, pub last_exit: Option<i32>, pub last_error: Option<String> }
  pub enum Command { Start(PathBuf), Stop(PathBuf), RestartBackend(PathBuf), Reconnect(PathBuf), Pause(PathBuf), Resume(PathBuf), Refresh, Quit }
  pub struct Supervisor { … }   // owns instances, policy per instance, clients
  impl Supervisor {
      pub fn new(spawner: Arc<dyn Spawner>, connector: Arc<dyn Connector>) -> (Self, watch::Receiver<Vec<InstanceView>>, mpsc::Receiver<Notice>);
      pub async fn run(self, commands: mpsc::Receiver<Command>);  // tick 500 ms
  }
  pub trait Connector: Send + Sync { async fn connect(&self, dir: &Path) -> io::Result<(ControlClient, mpsc::Receiver<EventLine>)>; } // async_trait-free: return a boxed future
  pub enum Notice { GaveUp { dir: PathBuf, name: String, code: Option<i32> }, NotSetUp { dir, name } }
  ```
  Behaviour: `set_instances(Vec<(PathBuf, bool autostart, Option<String> task)>)`; on
  start-up for each autostart instance: try `connect` first — reachable → External (not
  started by tray, never spawned); a task-run instance (task Some) is never spawned
  (Start runs the task via the Scheduler — injected); else spawn. After spawning, connect
  with retries every 500 ms (up to 30 s) and subscribe; events update state + history
  (`from_bridge`); a lost connection to a live child → state Starting and reconnect.
  Exit → `RestartPolicy::on_exit` → Failed{retry_in} / GaveUp (+Notice) / NotSetUp (+Notice) / Stopped.
  Stop: `shutdown` call (5 s) then wait up to 20 s for exit, then kill; state Stopped;
  requested_stop=true. External instance Stop: `shutdown` via the channel only.
  Quit: stop every `started_by_tray` instance (concurrently), leave the others, then return.
  RestartBackend/Reconnect/Pause/Resume: forwarded as calls; errors become `last_error`.
- `menu.rs`: pure `pub fn build(views: &[InstanceView], tray_autostart: bool, offer: &Offer, config_error: Option<&str>) -> Menu` where `Menu { items: Vec<Item> }`,
  `Item::Action { id: String, label: String, enabled: bool } | Item::Check { id, label, checked } | Item::Separator | Item::Submenu { label, items } | Item::Info(String)`.
  Per agent submenu "<name>": Info state label; Info "Folder: <folder>"; Info "Session:
  <id or none>"; separator; Start (enabled when Stopped/GaveUp/Failed/NotSetUp-with-config…
  not running); Stop (enabled when running/external); "Restart Coda"; "Reconnect";
  "Pause Coda for upgrade" (enabled when running and not paused); "Resume" (enabled when
  Paused); separator; "Open log"; "Open folder"; "Details…". NotSetUp → Info "Not set up:
  run keygen and enroll" and only Open folder / Details enabled. Ids:
  `"<action>:<instance id>"`. Top level: agent submenus, separator, "Add agent folder…",
  Check "Start at logon", the switch-over item "Switch from Scheduled Task to tray…" when
  offered, separator, "Quit".
  `pub fn parse_id(id: &str) -> Option<(MenuAction, String)>`.
- Tests: supervisor with a fake spawner/child and fake connector (duplex fake bridge):
  autostart spawns; an external reachable bridge is not spawned; exit 1 ×5 within 10 min →
  GaveUp + Notice; exit 2 → NotSetUp, no respawn; Stop sends shutdown and marks Stopped and
  does not restart; Quit stops only tray-started; events map to states (waiting → Waiting).
  Menu: ids round-trip, enabling rules, NotSetUp submenu, tooltip/worst icon from views.
- [ ] Gates + MSRV build; commit `feat(tray-core): supervisor, control client and menu model`.

## Task 9: Tauri shell `apps/push-agent-tray`

**Files:** root `Cargo.toml` (`exclude = ["apps/push-agent-tray"]` in `[workspace]`);
create `apps/push-agent-tray/{Cargo.toml, Cargo.lock, build.rs, tauri.conf.json,
capabilities/default.json, icons/icon.ico, icons/icon.png, src/main.rs, src/app.rs,
dist/details.html, dist/details.js, dist/details.css, README.md}`; `.github/workflows/ci.yml`
new job; `.gitignore` for `apps/push-agent-tray/target`.

- Cargo.toml: `[workspace]` (own), `edition = "2021"` or 2024, deps `tauri = { version =
  "2", features = ["tray-icon", "image-png"] }`, `tauri-plugin-notification = "2"`,
  `tauri-plugin-single-instance = "2"`, `tauri-plugin-dialog = "2"` (folder picker for
  "Add agent folder…"), `push-tray-core = { path = "../../crates/push-tray-core" }`,
  `push-bridge-control = { path = "../../crates/push-bridge-control" }`, `serde`,
  `serde_json`, `tokio`; build-dep `tauri-build = "2"`. `[features] custom-protocol =
  ["tauri/custom-protocol"]`, default `["custom-protocol"]` so a plain `cargo build`
  loads the bundled `dist`.
- tauri.conf.json: `productName "push-agent-tray"`, `identifier
  "com.push-messenger.agent-tray"`, `build.frontendDist "dist"`, no devUrl, `app.windows:
  []` (windows created on demand), `app.security.csp`: `"default-src 'self'; script-src
  'self'; style-src 'self'; img-src 'self'; connect-src ipc: http://ipc.localhost;
  object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'"`,
  `app.withGlobalTauri: false`, bundle targets `["msi"]`, `bundle.windows.webviewInstallMode`
  `downloadBootstrapper` default is fine; per-user install documented
  (`wix` `installMode`/perUser if supported, else portable exe).
- capabilities/default.json: windows `["details-*"]`, permissions `["core:default"]` only
  (plus nothing else: app commands are allowed for app-defined commands).
- main.rs: `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`; single
  instance; load tray.json (`ensure_default`, save), Scheduled Task scan
  (`PowerShellScheduler`, Windows only), start the `Supervisor`; tray icon from
  `push_tray_core::icon::rgba(worst, 32)` via `tauri::image::Image::new_owned`; rebuild the
  native menu from `menu::build` on every views change (watch) and on toggles; tooltip
  from `state::tooltip`; menu events → `parse_id` → Supervisor commands / open log
  (`<dir>\logs\bridge.log`) / open folder / details window / add folder (dialog picker,
  then `TrayConfig::add`, save, supervisor refresh) / Start at logon (RunKey toggle +
  start autostart instances) / switch-over (`plan_switch` + `execute_switch` after a
  native confirm dialog) / Quit (Supervisor Quit, then `app.exit(0)`). Notices →
  notification "<name> stopped after 5 failures (exit N). Open log from its menu." /
  "<name> is not set up: run keygen and enroll."
- Details window: `WebviewWindowBuilder::new(app, format!("details-{id}"),
  WebviewUrl::App("details.html".into()))` 420×520, title "<name> — details"; command
  `details(label: String) -> DetailsView` (derived from the window label) returning
  `{name, state, folder, session, codaVersion, runningTasks, lastError, lastExit,
  history: [{at, state}]}` — content-free. details.js renders with `textContent` only
  (never innerHTML), refreshes every 2 s via `window.__TAURI_INTERNALS__.invoke` (no
  global API, no CDN).
- icons: generate `icons/icon.png` (256×256) and `icons/icon.ico` from
  `push_tray_core::icon::rgba(Normal, …)` with a tiny one-off script (PowerShell
  System.Drawing is fine) and commit them.
- CI job:
  ```yaml
  tray-windows:
    runs-on: windows-latest
    timeout-minutes: 40
    steps:
      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4.4.0
      - uses: dtolnay/rust-toolchain@683b588c487a69d01f6c6ddf40f036ff682cc2c8 # master
        with: { toolchain: stable, components: clippy }
      - uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
        with: { workspaces: "apps/push-agent-tray -> target" }
      - run: cargo clippy --manifest-path apps/push-agent-tray/Cargo.toml --locked -- -D warnings
      - run: cargo build --manifest-path apps/push-agent-tray/Cargo.toml --release --locked
      - run: cargo test -p push-bridge-control -p push-tray-core -p push-bridge --locked --lib
  ```
  (the last line runs the Windows-only tests — pipe ACL, peer check, locked exe — in CI).
- [ ] Build locally: `cargo build --manifest-path apps/push-agent-tray/Cargo.toml --release`
  (WebView2 is installed). Clippy clean. Run the exe briefly only if safe (it would start
  the user's real bridges — **do not** run it against the real default folder; skip the
  run, building is enough).
- [ ] Commit `feat(tray): push-agent-tray Tauri shell` and `ci: build the tray on Windows`.

## Task 10: docs and changelog

**Files:** `docs/agent-bridge.md`, `CHANGELOG.md`, `README.md` (one line if it lists
binaries), `apps/push-agent-tray/README.md`.

agent-bridge.md: new sections — "The tray app (push-agent-tray)" (what it shows, icon
states table, per-agent menu, Start at logon, tray.json format and location, Scheduled
Task switch-over, restarts and failures, security, per-user install: portable exe next to
`push-agent-bridge.exe` or the MSI per user, no admin rights; building it:
`cargo build --manifest-path apps/push-agent-tray/Cargo.toml --release`); "Control
channel" (endpoints, instance id, access rules, NDJSON grammar with every request/
response/event and the status fields, `"control": false`, an example session, guidance
for other tools); "Upgrade mode" (the five steps, held text, announcement, bypass
re-confirmation, fallback for older Coda, unplanned kill → paused, the script
`push-agent-bridge pause; dotnet tool update -g coda; push-agent-bridge resume`, the Coda
contract fields parsed leniently); "Linux plan" (StatusNotifierItem/AppIndicator tray,
XDG autostart (implemented in tray-core) or a systemd user unit example, the socket path,
AppImage/deb later); the CLI table gains `pause`, `resume`; config table gains `control`;
"Manual smoke test on Windows" (the six steps from the request). CHANGELOG under
Unreleased/Added: control channel, upgrade mode, push-agent-tray.
- [ ] Commit `docs: agent tray, control channel and upgrade mode`.

## Self-review notes

Spec coverage: control channel (T1–T3), per-user + peer check (T2), events/no content
(T3), pause flow steps 1–5 + fallback + killed/locked (T4–T6), CLI pause/resume (T6),
tray (T7–T9), tray.json + auto default + add folder (T7, T9), Start at logon Run key / XDG
(T7, T9), Scheduled Task detect + switch-over never both (T7, T8, T9), supervision
backoff/give-up/exit 2 (T7, T8), Quit semantics (T8), Stop declines (T3), details window
(T9), CSP/no network (T9), packaging + docs + Linux plan (T9, T10), acceptance tests
(T3, T6, T7, T8), smoke steps documented (T10).
