# Voice adapter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move speech recognition and synthesis out of push-agent-bridge into a separate
voice adapter process reached over the versioned local protocol `push-voice-adapter/1`,
as three independently releasable PRs.

**Architecture:** PR 1 adds `crates/push-voice-adapter-proto` (framing, typed messages,
limits, version negotiation, JSON Schemas, fuzzing). PR 2 adds the bridge's adapter client
(`crates/push-bridge/src/voice/adapter/`: spawn and connect, handshake, liveness, routing
a call's audio and speech, chat-only fallback), runtime calls advertising in push-sdk, the
scripted test adapter `crates/push-voice-adapter-testkit`, and E2E tests in the
`bridge-voice` job. PR 3 removes the built-in providers and turns `calls.stt`/`calls.tts`/
`calls.vad` into startup errors pointing at `calls.adapter`. The contract is the design
addendum `docs/superpowers/specs/2026-11-04-voice-adapter-design.md` (cited "design §n" /
"design An") and the wire reference `docs/voice-adapter-protocol.md`.

**Tech Stack:** Rust 2024 (MSRV 1.85: no let-chains, no std API newer than 1.85), tokio,
serde/serde_json, schemars 1 (schemas), proptest (dev), cargo-fuzz (nightly, own
workspace), push-bridge-control's transport (named pipe DACL + SID check / Unix socket
0600), push-sdk `calls-media` (Rust 1.89 + CMake, only behind push-bridge's `voice`).

---

## Conventions for every PR

- Branch from `origin/main` in a new worktree; one PR per section; CHANGELOG entry under
  `## Unreleased` (`### Added` / `### Changed` / `### Removed`).
- Before pushing, run what CI runs (`.github/workflows/ci.yml`), on Windows with
  `$env:CARGO_INCREMENTAL=0` and `-j 4`:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings` and
    `cargo clippy -p push-bridge --all-targets --locked -- -D warnings` (without `voice`)
  - `cargo test --workspace --locked -j 4` (Docker running)
  - `cargo +1.85 build --workspace --locked -j 4`
  - `cargo deny check` when Cargo.lock changed
  - the env-gated E2E you touched: `$env:PUSH_BRIDGE_VOICE=1; cargo test -p push-smoke --test bridge_voice --locked -- --nocapture --test-threads 1`
  - `cargo test -p push-bridge --features voice --locked --lib -- voice::` (calls-media-windows equivalent)
- Never log, count by label, or put on the control channel: audio, transcript text,
  speech text, the peer's display name (design A15). Ids and codes are fine.
- Merge with `gh pr checks <n> --watch`, then `gh pr merge <n> --merge --delete-branch`.
- PR 3 lands only after PRs 1 and 2 are **released**.

---

# PR 1 — Protocol crate (framing, messages, schemas) with fuzzing

Delivered with this plan. Files:

- Create: `crates/push-voice-adapter-proto/{Cargo.toml, src/{lib,frame,io,tokio_io,message,version,format,limits,schema}.rs}`
- Create: `crates/push-voice-adapter-proto/tests/{common/mod.rs, messages.rs, properties.rs, schemas.rs}`
- Create: `crates/push-voice-adapter-proto/fuzz/{Cargo.toml, Cargo.lock, .gitignore, fuzz_targets/{frames,messages}.rs}`
- Create: `docs/schemas/voice-adapter/v1/*.json` (14 generated files)
- Create: `docs/voice-adapter-protocol.md`, this plan, the design addendum
- Modify: `Cargo.toml` (member, workspace dependency, `exclude` the fuzz dir), `Cargo.lock`,
  `.github/workflows/ci.yml` (job `voice-adapter-fuzz`), `README.md`, `CHANGELOG.md`

### Task 1.1: Frame codec

- [x] Tests in `src/frame.rs`: documented byte layout of both kinds; round trip and merged
  frames; byte-by-byte split; oversize refused from the header alone (nothing buffered,
  error sticks); the 65 536 limit accepted and +1 refused; unknown kinds 0/3/255, empty
  control, short/odd audio, unknown stream; `Debug` without payloads.
- [x] `Frame`, `AudioFrame`, `Kind`, `Stream`, `FrameError`, `FrameDecoder` (sans-IO).

### Task 1.2: Readers and writers

- [x] `io::{FrameReader, FrameWriter, ReadError, WriteError}` (blocking; short reads; EOF
  inside a frame is `UnexpectedEof`; invalid messages are not written).
- [x] `tokio_io::{AsyncFrameReader, AsyncFrameWriter}` behind default feature `tokio`;
  `read_frame` cancel-safe (test: a timed-out read loses nothing).

### Task 1.3: Messages, version, format, limits

- [x] `message`: `BridgeMessage`, `AdapterMessage`, all structs, `Capabilities`, `Voice`,
  `ErrorCode`/`CallEndReason` with `other`, `Decoded`, `decode`, `encode`, `Message`
  (`TYPES`, `message_type`, `validate`), `Invalid`/`Problem`, `MessageError` (content-free).
- [x] `version`: `PROTOCOL`, `SUPPORTED`, `ProtocolVersion::parse`, `negotiate`.
- [x] `format`: `AudioFormat::V1` (320 samples), `spec`, `from_spec`, `check`.
- [x] `limits`: every cap/timer of design A11–A14.
- [x] `tests/messages.rs`: the wire JSON of every message, unknown fields/types/enum
  values, malformed input never echoed, every cap on decode and encode, 64 KiB encode cap,
  `Debug` redaction, negotiation from both hellos.

### Task 1.4: Schemas, property tests, fuzzing, docs

- [x] `schema::schemas()` + `tests/schemas.rs` (regenerate-and-compare,
  `UPDATE_VOICE_ADAPTER_SCHEMAS=1`, no stale files, examples fit properties/required).
- [x] `tests/properties.rs` (proptest): any split/merge, arbitrary bytes never panic and
  re-encode identically, oversize headers, audio at the limit, JSON-ish noise, message
  round trips.
- [x] cargo-fuzz targets `frames` and `messages`; CI job `voice-adapter-fuzz` (nightly,
  60 s + 30 s).
- [x] `docs/voice-adapter-protocol.md` (complete wire reference + sequence diagram),
  README crate line, CHANGELOG.

---

# PR 2 — Bridge adapter client, testkit adapter, E2E

Branch `feat/voice-adapter-client`. Result: with `calls.adapter` configured the bridge
answers calls and every PR 7/8 voice behaviour works through the adapter; the built-in
providers still work when `calls.adapter` is absent (they are removed in PR 3).

**File map**

| File | Responsibility |
|---|---|
| `crates/push-sdk/src/client.rs` (+ `engine.rs`, the connection loop) | `Client::set_calls_advertised(bool)` |
| `crates/push-bridge/src/config.rs` | `CallsConfig.adapter: Option<AdapterConfig>`, validation |
| `crates/push-bridge/src/voice/adapter/mod.rs` | `AdapterState`, `AdapterInfo`, re-exports |
| `crates/push-bridge/src/voice/adapter/spawn.rs` | starting the child safely (design A18) |
| `crates/push-bridge/src/voice/adapter/connect.rs` | connecting to a pipe/socket (design A19) |
| `crates/push-bridge/src/voice/adapter/link.rs` | one live connection: hello, ping/pong, dead peer, rate caps, reader/writer tasks |
| `crates/push-bridge/src/voice/adapter/supervisor.rs` | the lifecycle of design §4: start → link → backoff, `watch::Sender<AdapterState>` |
| `crates/push-bridge/src/voice/adapter/router.rs` | one call over the link: audio both ways, speaks queue, events → `SessionEvent` |
| `crates/push-bridge/src/voice/engine.rs` | `SpeechEngine` trait; `BuiltinEngine` (today's VAD/STT/TTS moved out of `session.rs`) |
| `crates/push-bridge/src/voice/session.rs` | the line part only (media, speaker, generation, mute, events), speech delegated to an engine |
| `crates/push-bridge/src/core/voice.rs`, `core/calling.rs`, `core/report.rs` | fallback notes, adapter-aware status, advertising |
| `crates/push-bridge/src/main.rs` | wiring: supervisor, advertising, status line |
| `crates/push-bridge-control/src/protocol.rs` | additive `Status.adapter` |
| `crates/push-tray-core`, `apps/push-agent-tray` | show the adapter state |
| `crates/push-voice-adapter-testkit/` | scripted adapter library + binary |
| `crates/push-smoke/tests/bridge_voice.rs` | E2E through the testkit adapter |

### Task 2.1: push-sdk runtime calls advertising

**Files:** Modify `crates/push-sdk/src/client.rs` (the `calls: bool` config is read where
`Identify` is built, around `config.calls` at line ~401); test in the same file's tests.

- [x] **Step 1: failing test** `calls_advertising_changes_reconnect_with_the_new_capability`:
  start `Client::with_calls()`'s test server helper (client.rs ~954), connect, assert the
  server saw `CAPABILITY_CALLS` in `Identify`; call `client.set_calls_advertised(false)`;
  assert a new `Identify` without it arrives within 5 s; `set_calls_advertised(false)`
  again sends nothing.
- [x] **Step 2:** run `cargo test -p push-sdk --lib calls_advertising` → FAIL (no method).
- [x] **Step 3: implement** an `AtomicBool calls_advertised` initialised from
  `config.calls`, read when building `Identify`; `pub fn set_calls_advertised(&self, on:
  bool)`: swap; if changed and connected, close the socket with the normal reconnect path
  (no backoff penalty; reason `capabilities_changed`). Document: a reconnect is cheap and
  `CallSync` (voice spec D7) restores call state; never call it during a call.
- [x] **Step 4:** test passes; `cargo test -p push-sdk --lib` green.
- [x] **Step 5:** commit `feat(push-sdk): change calls advertising at runtime`.

### Task 2.2: `calls.adapter` config

**Files:** Modify `crates/push-bridge/src/config.rs` (`CallsConfig` at line ~72, its
`validate`), tests in the same file.

```rust
/// Where speech runs (docs/voice-adapter-protocol.md §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AdapterConfig {
    Spawn(SpawnConfig),
    Connect(ConnectConfig),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnConfig {
    /// argv: program then arguments; never run through a shell.
    pub command: Vec<String>,
    /// `log` (default: <state dir>/logs/voice-adapter.log) or `discard`.
    #[serde(default)]
    pub stderr: StderrMode,
    /// Extra variables to pass even though they look secret (rarely needed).
    #[serde(default)]
    pub env_allow: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ConnectConfig {
    /// Windows: `\\.\pipe\<name>`; the name only, 1–200 chars of [A-Za-z0-9._-].
    Pipe(String),
    /// Unix: an absolute path after `$XDG_RUNTIME_DIR` expansion.
    Socket(String),
}
```

`CallsConfig` gains `#[serde(default)] pub adapter: Option<AdapterConfig>`.

- [x] **Step 1: failing tests** (`config.rs` tests): parses
  `{"adapter":{"spawn":{"command":["C:\\a.exe","--x"]}}}` and
  `{"adapter":{"connect":{"pipe":"push-voice-me"}}}`; refuses with field-named errors:
  empty `command` (`calls.adapter.spawn.command is empty`), an empty argument, a pipe name
  with `\` or spaces, `socket` on Windows / `pipe` on Unix (`calls.adapter.connect.pipe is
  only available on Windows`), `adapter` together with `stt` other than `none`
  (`calls.adapter and calls.stt cannot both be set; speech runs in the adapter`), unknown
  keys. A relative program path is accepted (warning logged at startup, not a test here).
- [x] **Step 2:** `cargo test -p push-bridge --lib config::` → FAIL.
- [x] **Step 3:** implement the types and `validate_adapter(&AdapterConfig) -> Result<(), String>`.
- [x] **Step 4:** PASS. **Step 5:** commit `feat(push-bridge): calls.adapter config`.

### Task 2.3: Spawning the adapter safely

**Files:** Create `crates/push-bridge/src/voice/adapter/spawn.rs`, `mod.rs`.

```rust
pub struct Spawned {
    pub child: tokio::process::Child,         // kill_on_drop(true)
    pub stdin: tokio::process::ChildStdin,    // bridge → adapter
    pub stdout: tokio::process::ChildStdout,  // adapter → bridge
}

/// Variables never passed to the adapter: exact names and prefixes.
pub const SECRET_VARS: &[&str] = &["CODA_SERVE_API_KEY", "AZURE_SPEECH_KEY"];
pub const SECRET_PREFIXES: &[&str] = &["PUSH_", "CODA_"];
/// Also stripped: names containing KEY, TOKEN, SECRET or PASSWORD (case-insensitive).

pub fn adapter_env(vars: impl Iterator<Item = (OsString, OsString)>, allow: &[String])
    -> Vec<(OsString, OsString)>;

pub fn spawn(config: &SpawnConfig, log_dir: &Path) -> std::io::Result<Spawned>;
```

`spawn` uses `tokio::process::Command::new(&command[0]).args(&command[1..])`,
`env_clear()` + `envs(adapter_env(std::env::vars_os(), &config.env_allow))`,
`stdin/stdout(Stdio::piped())`, stderr to an append-only file created with user-only
permissions (reuse `logging.rs`'s file helper) or `Stdio::null()`, `kill_on_drop(true)`,
Windows `creation_flags(CREATE_NO_WINDOW)` (0x0800_0000) and the child assigned to a
job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (reuse `exe/windows.rs` patterns;
`unsafe` stays confined there).

- [x] **Step 1: failing tests**: `adapter_env` drops `CODA_SERVE_API_KEY`, `PUSH_AGENT_KEY`,
  `MY_API_TOKEN`, `db_password`, keeps `PATH`, `SystemRoot`, `HOME`, `LANG`, keeps an
  allowed `MY_API_TOKEN` with `env_allow`; `spawn` of a test program prints its argv and
  environment: an argument `"a b; echo pwned"` arrives as one argument (no shell), the
  secret variable is absent. Test program: the testkit binary's `--dump-env` mode
  (Task 2.8) — until then, a `#[cfg(test)]` use of `std::env::current_exe()` with a
  `--dump-env` test hook is acceptable.
- [x] **Step 2–4:** fail, implement, pass. **Step 5:** commit.

### Task 2.4: Connecting to an adapter's endpoint

**Files:** Create `crates/push-bridge/src/voice/adapter/connect.rs`.

```rust
pub fn endpoint(config: &ConnectConfig) -> Result<push_bridge_control::Endpoint, String>;
// Pipe(name) → Endpoint::Pipe(format!(r"\\.\pipe\{name}"));
// Socket(path) → Endpoint::Socket(expand_xdg(path)?)
pub async fn connect(config: &ConnectConfig) -> std::io::Result<push_bridge_control::transport::Stream>;
```

`connect` is `push_bridge_control::transport::connect(&endpoint)`; it already checks the
server's user (SID or uid). Map `NotFound` → "no voice adapter is listening on <name>",
`PermissionDenied` → "<name> belongs to another user", others as is (content-free).

- [x] **Step 1: failing tests**: endpoint naming; a `push_bridge_control::transport::Listener`
  bound on a unique name accepts our connection and frames cross both ways
  (`AsyncFrameWriter`/`AsyncFrameReader` from push-voice-adapter-proto); nobody listening
  is `NotFound` with the friendly message. (Windows: also runs in `tray-windows`'s
  `cargo test -p push-bridge --lib`.)
- [x] **Step 2–4.** **Step 5:** commit.

### Task 2.5: The link — handshake, liveness, caps

**Files:** Create `crates/push-bridge/src/voice/adapter/link.rs`. Add
`push-voice-adapter-proto.workspace = true` to `crates/push-bridge/Cargo.toml`.

```rust
pub struct AdapterInfo { pub name: String, pub version: String,
                         pub capabilities: Capabilities, pub voices: Vec<Voice> }

pub enum LinkEvent {               // to the router / supervisor
    Message(AdapterMessage),        // validated, never hello/ping/pong
    Audio(AudioFrame),              // stream 1 only, format-checked
    Closed(LinkEnd),                // exactly once
}
pub enum LinkEnd { Eof, DeadPeer, Overloaded, Framing(String), Protocol(String), Io(String) }

pub struct Link { to_adapter: mpsc::Sender<Outgoing>, audio: AudioQueue, info: AdapterInfo }
impl Link {
    pub async fn open<R, W>(reader: R, writer: W, hello: BridgeHello)
        -> Result<(Link, mpsc::Receiver<LinkEvent>), LinkEnd>;   // HELLO_TIMEOUT
    pub fn send(&self, message: BridgeMessage);                  // control, never dropped
    pub fn send_audio(&self, samples: &[i16; 320]);              // stream 0, 1 s drop-oldest
    pub fn info(&self) -> &AdapterInfo;
}
```

Rules to implement (design A6, A7, A11–A13): send hello, wait ≤ 10 s for the adapter's;
reject a protocol not offered (`unsupported_protocol`, close); `error` instead of hello →
`LinkEnd::Protocol(code)`; ping every 10 s, answer pings, 30 s without any frame →
`DeadPeer`; > 100 control messages in any 1 s window → send `overloaded`, close;
partials beyond 10/s dropped; malformed messages answered `invalid_message` (count only);
unknown types logged once per name (`tracing::debug!(event = "adapter_unknown_type",
name)`); stream-0 frames from the adapter or wrong-size frames dropped (+ one
`unsupported_format` per call); framing error → `invalid_frame`, close. Writer task: one
`select!` with control first (`biased;`), then audio from a `VecDeque<[i16;320]>` capped
at `TO_ADAPTER_QUEUE_FRAMES` (drop oldest, count `dropped_to_adapter`). Seq/timestamp of
stream 0 per call (reset at `call_started`).

- [x] **Step 1: failing tests** (paused tokio time, `tokio::io::duplex`, a hand-driven fake
  adapter using push-voice-adapter-proto): hello exchange gives `AdapterInfo`; no hello in
  10 s → `Closed(Protocol("hello timeout"))`; a `push-voice-adapter/2` answer →
  `unsupported_protocol` sent and closed; ping answered with the same id; silence 30 s →
  `DeadPeer`; 101 messages in a second → `overloaded` and closed; an oversize header →
  `invalid_frame`; an unknown type is ignored and the link stays up; with a stalled
  reader the 51st queued audio frame drops the oldest and control still goes first; logs
  captured with a tracing test subscriber contain no text from a transcript.
- [x] **Step 2–4.** **Step 5:** commit.

### Task 2.6: The supervisor — lifecycle and state

**Files:** Create `crates/push-bridge/src/voice/adapter/supervisor.rs`.

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterState {
    Off,
    Starting,
    Handshaking,
    Ready(AdapterInfo),
    Backoff { until: Instant, reason: String },   // content-free reason
}
pub struct Supervisor { state: watch::Receiver<AdapterState>, links: mpsc::Receiver<(Link, mpsc::Receiver<LinkEvent>)> }
pub fn start(config: AdapterConfig, hello: BridgeHello, log_dir: PathBuf) -> Supervisor;
```

Loop: Starting → spawn/connect → Handshaking → `Link::open` → Ready (hand the link to the
core) → wait for `Closed` → (spawn: kill child, wait 2 s) → Backoff with
`crate::backoff::Backoff::new()` (1 s doubling to 60 s; `reset()` after 60 s Ready,
`RESET_AFTER`) → Starting. Restarts counted (`adapter_restarts`). Shutdown: send
`call_ended {shutdown}` if a call runs, close, wait 2 s, kill.

- [x] **Step 1: failing tests** (paused time, the testkit library adapter over duplex
  streams via an injectable `Starter` trait so no process is needed): states in order
  Starting → Handshaking → Ready; adapter closes → Backoff 1 s → Ready again; three quick
  failures → 1 s, 2 s, 4 s; 60 s Ready then failure → 1 s again.
- [x] **Step 2–4.** **Step 5:** commit.

### Task 2.7: SpeechEngine split and the adapter router

**Files:** Create `crates/push-bridge/src/voice/engine.rs`,
`crates/push-bridge/src/voice/adapter/router.rs`; modify `voice/session.rs`,
`voice/mod.rs` (`VoiceSetup` gains `adapter: Option<watch::Receiver<…>>`, `stt`/`tts`
stay until PR 3).

```rust
/// Speech for one call: hears the line's audio, reports SessionEvents, speaks sentences.
pub trait SpeechEngine: Send + Sync + 'static {
    /// The caller's frame (from the line's incoming audio).
    fn hear(&self, frame: &Frame);
    /// Queue a sentence; `own` = the bridge's own line (not interruptible, never muted).
    fn say(&self, text: String, own: bool);
    /// Stop speaking now (barge-in, hush, mute, hang-up): drop the queue.
    fn stop(&self);
    /// The call is over.
    fn end(&self, reason: CallEndReason);
}
```

`Session` keeps: the line, `Speaker`, generation/mute/silence, `SessionEvent` emission,
`speaking()`. `BuiltinEngine` = today's `listen`/`transcribe`/`speak` tasks moved from
`session.rs` unchanged (all existing `session.rs` tests must still pass unmodified except
for construction). `AdapterEngine` (router.rs):

- `call_started {call_id: random 16 hex, peer: display name ≤ 64 chars, language}` on start
  and again on a new link during the call (design A21);
- `hear` → `link.send_audio`;
- `speech_started` → record `started = Instant::now()` per `utterance_id`; emit
  `Listening(true)`; if speaking: the session's barge-in (generation bump, `clear()`,
  mute reply, `BargedIn`, `Speaking(false)`) and `stop_speaking {call_id}`;
- final `transcript` → `Listening(false)` + `Transcript { text, started }` (started from
  its `speech_started`, else now); partials → `Listening(true)` only; duplicates ignored;
- `say` → a `speak {speak_id: s<n>, text, interruptible: !own}`; at most 16 outstanding,
  the rest queued locally; `Speaking(true)` at the first stream-1 frame;
- stream-1 frames → the line's `Speaker` (2 s cap, drops counted, frames of a stopped
  speak dropped); `speak_done` → next; `Speaking(false)` when the queue is empty and the
  last frame has played (the existing `playing_until` logic);
- `recognition_failed` → `SpeechFailed("recognition_failed")`;
  `synthesis_failed` → skip; `engine_unavailable` → `AdapterUnavailable` event;
- link closed → `SessionEvent::AdapterLost`; new link → `AdapterBack` (new variants).

- [x] **Step 1: failing tests** (paused time, `FakePhone` + testkit library adapter): the
  owner's tone becomes a `Transcript`; a sentence becomes `speak` and its tone plays on
  the fake call; barge-in sends `stop_speaking` and silences the call within 40 ms of
  `speech_started`; 20 sentences keep ≤ 16 outstanding; `own` lines are
  `interruptible: false`; final transcript → `Transcript` event < 100 ms (paused clock);
  stream-1 playback has no gap > 40 ms with a well-behaved adapter; link loss mid-speech
  → `AdapterLost`, silence, no panic; a new link → `call_started` with the same id.
- [x] **Step 2–4.** **Step 5:** commit.

### Task 2.8: Core integration, advertising, status, tray, metrics

**Files:** Modify `crates/push-bridge/src/core/voice.rs`, `core/calling.rs`,
`core/report.rs`, `core/voice_tests.rs`, `main.rs` (`voice_setup`, `advertises_calls`,
`voice_status` at lines ~214–280), `crates/push-bridge-control/src/protocol.rs`,
`crates/push-tray-core`, `apps/push-agent-tray/src/*` (Overview text).

- Core: `AdapterLost` → post `agent.reply@1` "Voice adapter stopped; reconnecting…", stop
  speaking, keep the call; replies keep going to the chat only; `AdapterBack` → "Voice
  adapter is back."; `AdapterUnavailable` → "Voice adapter cannot listen right now." and
  chat-only until the next call. Everything else (prompt/steer, card matching, speech
  text, masking, activity, transcripts, tray mute/hang-up) unchanged.
- Advertising (design A20): a task watches `AdapterState`; advertise when `Ready` with
  `stt` for 2 s and no call/ask/dial is in progress; withdraw at once when not ready and
  no call runs; otherwise defer until the call ends. `Client::set_calls_advertised`.
- `status`: `Voice: adapter ready (name 1.2.3; stt, tts, vad)` / `adapter starting` /
  `adapter backing off 4 s (no voice adapter is listening on push-voice-me)`.
- Control channel (additive): `Status.adapter: Option<AdapterStatus {state, name?,
  version?, capabilities?}>`, `adapter_changed` event; protocol tests classify it
  content-free.
- Tray: Overview line "Voice adapter: name 1.2.3 (ready)" or the state.
- Metrics (log lines `event = "adapter_metrics"` at call end and in `status`): frames in/out,
  dropped by direction, restarts, speak latency last/avg/max.

- [x] **Step 1: failing tests** in `core/voice_tests.rs`: with a Ready adapter the owner's
  call is answered and a transcript becomes a prompt through `TestBackend`; adapter lost
  mid-call → the note is posted and the call stays; back → note, `call_started` again;
  no adapter → `advertised == false` and an incoming call event is ignored; advertising
  does not change during a call. Control protocol test for `adapter` content-freeness.
  Tray-core view-model test for the adapter line.
- [x] **Step 2–4.** **Step 5:** commit.

### Task 2.9: Testkit adapter

**Files:** Create `crates/push-voice-adapter-testkit/{Cargo.toml, src/lib.rs, src/main.rs,
src/script.rs, src/tones.rs}`; workspace member.

- Library `ScriptedAdapter::run(reader, writer, script)` over any `AsyncRead/AsyncWrite`.
- `Script` (JSON): `name`, `version`, `capabilities`, `words: [{hz, word}]` (Goertzel over
  stream 0, the PR 7 fixtures' rules: 350–2400 Hz, 200 Hz apart, never within 150 Hz of
  600 Hz — reuse `push_bridge::voice::fake`'s detector by moving it here in PR 3; in PR 2
  copy the Goertzel function), `speak_hz: 600`, `ms_per_char: 40`, faults:
  `crash_after_frames`, `refuse_format`, `protocol` (answer another version),
  `stall_after_hello`, `flood_messages`, `no_hello`.
- Binary `push-voice-adapter-testkit --script <file> [--pipe <name> | --socket <path>]
  [--dump-env]`: stdio by default; connect mode listens with
  `push_bridge_control::transport::Listener` (same DACL/SID rules) and answers a second
  bridge `busy`.
- [x] **Step 1: failing tests**: the library against push-voice-adapter-proto readers: a
  tone fixture becomes `speech_started` + final transcript of the scripted words; `speak`
  returns 600 Hz frames for `len × ms_per_char` then `speak_done`; `stop_speaking` →
  `speak_done {interrupted: true}`; each fault behaves as named.
- [x] **Step 2–4.** **Step 5:** commit.

### Task 2.10: E2E through the testkit adapter, CI

**Files:** Modify `crates/push-smoke/tests/bridge_voice.rs`, `crates/push-smoke/Cargo.toml`
(dev-dep testkit), `.github/workflows/ci.yml` (`bridge-voice` builds the testkit binary
first: `cargo build -p push-voice-adapter-testkit --locked`).

Scenarios (each a `step(...)` in the existing style, `PUSH_BRIDGE_VOICE=1`):

- [x] spawn mode: call → `call_started` → audio flows → a final transcript becomes a Coda
  prompt through the scripted backend; the reply is spoken (600 Hz heard by the owner's
  `MediaCall`), barge-in stops it; a voice answer closes a card (existing assertions);
- [x] connect mode: the testkit listening on a unique pipe/socket, same assertions (short);
- [x] crash mid-call (`crash_after_frames`) → the "Voice adapter stopped; reconnecting…"
  message reaches the owner, the call stays, after restart a new utterance becomes a
  prompt;
- [x] no adapter (`calls.adapter` pointing at a missing program) → the owner's call is
  `UNREACHABLE`;
- [x] format mismatch (`refuse_format`) → the bridge's log has one
  `unsupported_format` line and calls are not advertised;
- [x] timing: final transcript → `speak` < 100 ms excluding the backend (timestamps from the
  testkit's log of received frames); owner-side gaps in the agent's audio ≤ 40 ms;
- [x] content-free logs: the bridge's log file contains none of the scripted words or
  spoken text (the existing helper in this file).
- [x] Commit; push; `gh pr checks --watch`.

### Task 2.11: Docs and changelog

- [x] `docs/agent-bridge.md` §18: new subsection "Voice adapter" (config, spawn/connect,
  status, fallback) — the built-in providers are documented as deprecated.
- [x] `docs/voice-adapter-protocol.md`: Status paragraph updated (the client shipped).
- [x] Design §12 "PR 2: decisions and deviations"; CHANGELOG `### Added`.

---

# PR 3 — Remove the built-in providers, migration errors, docs

Branch `feat/voice-adapter-only`. Lands only after PRs 1 and 2 are released.

### Task 3.1: Migration errors first (TDD)

**Files:** Modify `crates/push-bridge/src/config.rs`.

- [ ] **Step 1: failing tests**: a config with `calls.stt` (any value, `none` included),
  `calls.tts` or `calls.vad` fails to load with exactly:
  `calls.stt is no longer supported: speech runs in a voice adapter. Configure
  calls.adapter (docs/agent-bridge.md §18, docs/voice-adapter-protocol.md).` (and the same
  for `tts`/`vad`); a `calls` section without `adapter` loads, calls are not advertised and
  `status` says `Voice: off (no calls.adapter)`.
- [ ] **Step 2:** FAIL. **Step 3:** keep `stt`/`tts`/`vad` as
  `Option<serde_json::Value>` fields used only to produce the error (so `deny_unknown_fields`
  does not hide the helpful message). **Step 4:** PASS. **Step 5:** commit.

### Task 3.2: Remove the providers

- [ ] Delete `voice/{azure,local,provider,vad,wav,test_server}.rs` and `BuiltinEngine`;
  move `voice/fake.rs`'s Goertzel and fixtures into `crates/push-voice-adapter-testkit`
  (the testkit then owns the PR 7 fixture rules), update `tests/fixtures/voice` users.
- [ ] `VoiceSetup` loses `stt`, `tts`, `vad`; `Session` uses the adapter engine only.
- [ ] Drop `reqwest` from push-bridge if no other module uses it
  (`cargo tree -p push-bridge -i reqwest`); `cargo update --workspace --offline`.
- [ ] Rewrite affected `core/voice_tests.rs` tests onto the testkit library adapter.
- [ ] Full checks (Conventions); commit.

### Task 3.3: Docs

- [ ] `docs/agent-bridge.md` §18 rewritten around adapters (providers, "Privacy: what each
  provider receives" → "what the adapter receives"); migration note.
- [ ] `docs/calls.md` §19 updated.
- [ ] `docs/voice-adapter-protocol.md`: add "Writing an adapter" (pseudo-code main loop:
  read hello → answer → loop over frames: audio → VAD/STT, speak → queue TTS, pacing
  with a 20 ms clock, ping/pong, call_ended → reset) and "Troubleshooting" (no audio;
  adapter not found / not on PATH; pipe in use — `FILE_FLAG_FIRST_PIPE_INSTANCE`;
  `belongs to another user`; version mismatch; stdout polluted by prints).
- [ ] Voice spec §10.3 marked superseded by the addendum; design §12 "PR 3" notes;
  CHANGELOG `### Removed` (built-in providers) and `### Changed` (migration).
- [ ] push-sdk docs: `push_sdk::media`'s 16 kHz mono PCM named as the adapter format.
- [ ] Commit, PR, CI green, merge.
