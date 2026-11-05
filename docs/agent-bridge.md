# Agent bridge: a local coding agent as a private contact

`push-agent-bridge` (crate `push-bridge`) lets the owner of an AI coding agent that runs on
their own Windows machine reach it from a phone or a browser through push-messenger. The
agent becomes a private [owned agent](entra-sign-in.md#owned-agents-and-app-only-authentication-opt-in)
contact: the owner sends it tasks, watches it think and work, approves its edits and
commands, answers its questions and approves its plans from cards, and controls the session
with `/` commands. Coda is the first backend; `echo` is a deterministic test agent
([echo backend](#19-echo-backend)). Design: the implementation plan
[2026-10-29-agent-bridge](superpowers/plans/2026-10-29-agent-bridge.md). Where this guide
and the plan differ, this guide describes what the code does.

- [1. Overview](#1-overview)
- [2. Architecture](#2-architecture)
- [3. Requirements](#3-requirements)
- [4. Setup on Windows](#4-setup-on-windows)
- [5. Command line](#5-command-line)
- [6. Behaviour](#6-behaviour)
- [7. Cards](#7-cards)
- [8. Commands](#8-commands)
- [9. Security model](#9-security-model)
- [10. Files](#10-files)
- [11. Troubleshooting](#11-troubleshooting)
- [12. Live smoke test](#12-live-smoke-test-opt-in)
- [13. The tray app (push-agent-tray)](#13-the-tray-app-push-agent-tray)
- [14. Control channel (for other tools)](#14-control-channel-for-other-tools)
- [15. Upgrade mode](#15-upgrade-mode)
- [16. Linux plan](#16-linux-plan)
- [17. Manual smoke test on Windows](#17-manual-smoke-test-on-windows)
- [18. Voice calls](#18-voice-calls)
- [19. Echo backend](#19-echo-backend)

Design of the tray, the control channel and upgrade mode: the plan
[2026-11-02-agent-tray-upgrade-mode](superpowers/plans/2026-11-02-agent-tray-upgrade-mode.md);
here too, this guide describes what the code does.

## 1. Overview

| | |
|---|---|
| What runs | one `push-agent-bridge.exe run` per agent, as the logged-on user, started at logon by a Scheduled Task or by the [tray app](#13-the-tray-app-push-agent-tray) (never both) |
| Local control | each bridge serves a per-user [control channel](#14-control-channel-for-other-tools) (a named pipe): states and counts out, `restart_backend`, `reconnect`, `shutdown`, `pause_backend` and `resume_backend` in; never message content |
| Identity | an owned agent signed in with [agent-key](agent-key-sign-in.md): a non-exportable P-256 key in the TPM (or the software key storage provider) |
| Who may use it | only the agent's owner (`owner_user_id` of the agent's own profile); anyone else gets "This agent is private." |
| The agent | `coda serve` over stdio (JSON-RPC 2.0 with Content-Length framing), started and restarted by the bridge |
| Replies | `agent.reply@2` cards (Coda writes Markdown; `agent.reply@1` for a plain-text backend): thinking, tool steps and text, edited as the turn runs |
| Questions to the owner | `agent.approval@1`, `agent.question@1`, `agent.choose@1`, `agent.plan-approval@1` ([built-ins](rich-messages.md#6-built-in-templates)) |
| Status | live activity: working, waiting, idle ([activity](activity-presence.md#7-agents)) |
| Voice calls | the owner may call the agent from their app (build with `--features voice`, a `calls` section): what they say becomes prompts, posted as `agent.transcript@1`; the replies are spoken; the speech runs in a separate voice adapter process (any engine, local or cloud); cards can be answered by voice ([voice calls](#18-voice-calls)) |
| Stored on the machine | `config.json`, `state.json` (no secrets) and a JSON-lines log; tokens stay in memory |

## 2. Architecture

```
 owner's app ──push-messenger──▶ push-sdk Client ──▶ SdkMessenger ┐
                                                                  ▼
                       shared core (crate push-bridge, module core)
          owner gate · replies · cards + CardBook · activity · masking
          commands · sessions per folder · restart backoff · state
                                      │ Backend trait      ▲ BackendEvent
                                      ▼                    │
                              CodaBackend ──stdio──▶ coda serve --cwd <folder>
```

**The shared core** (`core`, `render`, `cards`, `commands`, `mask`, `folders`, `state`,
`config`, `backoff`, `logging`) knows nothing about Coda. It signs in, checks who is
talking, renders turns, posts cards and checks their answers, sends activity, masks
everything it sends, runs the bridge's own commands, keeps the state and restarts the
backend with backoff. It talks to push-messenger through a small `Messenger` trait
(`SdkMessenger` over a signed-in `push_sdk::Client`).

**The backend** implements `push_bridge::backend::Backend`:

| Call | Meaning |
|---|---|
| `name()` | the name in replies, e.g. "(Coda stopped; restarting)" |
| `capabilities()` | what it can do beyond prompts: `Steer`, `PlanApproval`, `Tasks`, `Sessions`, `Fork`, `Compact`, `Models`, `Effort`, `PermissionModes`, `Goal`, `AskAside`, `Schedules`, `Skills`, `AgentMessages`; a command whose capability is missing answers "Not supported by <name>." |
| `start(folder, session, permission_mode)` | start (or restart) the agent in a folder and open a session or a new one; `SessionNotFound` when the session is unknown there |
| `stop()` | stop the agent |
| `prompt(text)` | run one turn; `Busy` while a turn runs |
| `steer(text)` | add text to the running turn: accepted, `NoActiveTurn`, `TurnEnding` or rejected |
| `interrupt()` | stop the running turn |
| `command(BackendCommand)` | compact, models, set model/effort/permission mode/goal, sessions, fork, tasks, stop a task, ask aside, schedules, skills; returns plain text the core masks |
| `status()` | model, version and running tasks for `/status` |
| `pending_messages(after, engine)` | background messages missed after a cursor |
| `answer_timeout()` | how long a card waits for the owner (default 10 minutes) |
| `activity_labels()` | the activity labels while working with no tool running (default `thinking`) and while a question is open (default `question`) |
| `skip_texts_older_than()` | owner texts stored more than this before the bridge started are not delivered (default: all are) |

The backend reports through an unbounded channel of `BackendEvent`s: turn started (by a
prompt or by the engine) and ended (interrupted or not), thinking and text deltas, tool
started and finished (with the subagent's task, if any), a request (permission, question or
plan) with a `Responder` to answer it (dropping it declines), background messages, task
started and completed, errors, and `Exited` when the process stops.

**`CodaBackend`** (`coda` module) runs `<exe> serve <args…> --cwd <folder> --permission-mode
<mode>` with the arguments as a list (never through a shell), stdin and stdout piped,
stderr discarded, no console window, and `CODA_SERVE_API_KEY` removed from its
environment. It always sends `initialize` with `protocolVersion: "1"`,
`clientCapabilities: {stateEvents: true}` and the stored `sessionId`, maps Coda's
notifications to events, answers `request/permission`, `request/question` and
`request/planApproval` directly (or with error -32800 to decline), and maps the owner's
commands to `session/*`, `task/stop` and `skills/list`. One process serves one folder; a
`/cd` restarts it with the new `--cwd`.

**`TestBackend`** (`test_backend` module) is a scripted backend that records calls and emits
the events a test asks for. The core's tests and the integration tests
(`crates/push-bridge/tests/bridge.rs`) run the core against it and a `TestMessenger`,
which shows that the core runs without Coda. `cargo test -p push-bridge` runs them all,
including a fake `coda serve` for the Coda backend.

**Adding a backend** (Cortex, for example) needs no second bridge (`EchoBackend`, module
`echo`, is a small example):

1. Implement `Backend` in a new module: map the agent's protocol to the calls and events
   above, report only the capabilities it has, and answer each `Responder`.
2. Add a `BackendKind` variant and its config section next to `coda` in `config.rs`
   (`"backend": "<name>"`, `"<name>": {…}`), with `Config::validate` checking the section.
3. Build the backend for that kind in `main.rs`, where `CodaBackend::new` is built today.
4. Test it against the core with the core's tests as a model.

**The control channel** (crate `push-bridge-control`: the wire protocol, the endpoint names
and the per-user transport) is served by `push_bridge::control::ControlHub`: the core
publishes its state and content-free events into it and takes the channel's actions from
it. **`push-tray-core`** holds everything the tray app does that can be tested without a
UI (states, icons, restart policy, `tray.json`, start at logon, Scheduled Tasks, the
supervisor and the menu model); **`apps/push-agent-tray`** is the Tauri v2 shell around
it, a Cargo workspace of its own outside the root one.

**Voice** (`voice` module, [voice calls](#18-voice-calls)): a `Phone` answers the owner's
calls (`MediaPhone` over push-sdk's media helper, feature `voice`; `FakePhone` in tests),
a `Session` per call runs the pipeline (the owner's audio out, transcripts in, the
sentences to speak, barge-in) with its speech in a `SpeechEngine`: the voice adapter's
(`voice::adapter`: spawn or connect, the link, the supervisor and the call router; the
bridge has no speech engine of its own). The core's `core/voice.rs`
answers or declines calls, posts transcripts, sends them to the backend like typed text,
speaks replies and cards, and takes spoken answers; nothing in it knows Coda.

## 3. Requirements

- Windows 10 or 11, or a Windows 365 Cloud PC (no managed identity needed). A TPM 2.0 is
  preferred; without a usable TPM the key goes to the Microsoft Software Key Storage
  Provider.
- Coda installed (`coda.exe` with `coda serve`), signed in or configured for its model as
  you would for its terminal UI. The bridge never sets Coda's credentials.
- A push-messenger deployment with `agent-key` in `PUSH_AUTH_PROVIDERS`
  ([enable it](agent-key-sign-in.md#enable-it)), `PUSH_AGENTS_PER_USER` above 0, and
  message-svc of this release or later (it seeds `agent.choose@1` and
  `agent.plan-approval@1`).
- An owner account on that deployment, and an app that renders cards and sends answers.
- To build: Rust 1.85 or later and the MSVC build tools (push-sdk compiles its bundled
  SQLite); with `--features voice` (voice calls), Rust 1.89 and CMake.
- For [voice calls](#18-voice-calls): calls enabled on the deployment
  ([enabling calls](calls.md#2-enabling-calls)), a message-svc that seeds
  `agent.transcript@1`, an owner's app that places calls, and a
  [voice adapter](#voice-adapter) program.

## 4. Setup on Windows

The steps use the default folder `%LOCALAPPDATA%\push-messenger\coda-bridge`. Add `--dir
<folder>` to every command to use another one.

### 1. Build the bridge

```powershell
cargo build --release -p push-bridge
Copy-Item target\release\push-agent-bridge.exe C:\Tools\push-agent-bridge.exe
```

For voice calls with the owner (the `calls` section), build with `--features voice`
(push-sdk's media helper: Rust 1.89 and CMake). A build without it ignores `calls`, warns
once, and does not advertise calls, so the owner's app shows the agent as unreachable.
`status` prints a `Voice:` line saying which applies ([voice calls](#build-with-voice)).

Keep the program somewhere stable: the logon task runs it from that path.

### 2. Write `config.json`

Create `%LOCALAPPDATA%\push-messenger\coda-bridge\config.json`. Every field:

```json
{
  "api_url": "https://push.example.com",
  "socket_url": "wss://push.example.com/v1/socket",
  "host": null,
  "audience": "https://push.example.com",
  "allow_insecure_transport": false,
  "key": { "name": "push-messenger-coda-bridge", "storage": "windows" },
  "device_name": "BUILD-PC",
  "roots": ["C:\\src", "D:\\work"],
  "default_folder": "C:\\src\\app",
  "default_permission_mode": "default",
  "backend": "coda",
  "coda": { "exe": "C:\\Tools\\coda\\coda.exe", "args": [] },
  "control": true,
  "peek": true
}
```

| Field | Required | Meaning | Default |
|---|---|---|---|
| `api_url` | yes | auth-svc's public URL; `https://` (or `http://`) | |
| `socket_url` | no | the gateway's WebSocket; `wss://` (or `ws://`) | `api_url` with `https` → `wss` (`http` → `ws`) and `/v1/socket` appended |
| `host` | no | a `Host` header override, for development clusters reached by address | none |
| `audience` | no | the `aud` of every assertion: auth-svc's `PUSH_AUTH_AGENT_KEY_AUDIENCE` | `api_url` without a trailing `/` |
| `allow_insecure_transport` | no | allow `http://` and `ws://` to hosts other than this machine (development only) | `false` |
| `key.name` | no | the key's name in Windows key storage: one key per agent; not empty | `push-messenger-coda-bridge` |
| `key.storage` | no | `windows` (CNG: the TPM, else the software provider; never exportable) or `software-file` (`<dir>\agent-key.p8`, a plain file, for development and tests only) | `windows` |
| `device_name` | no | the device's name on the server | `%COMPUTERNAME%`, else `push-agent-bridge` |
| `roots` | yes | the folders the agent may work in and below; at least one | |
| `default_folder` | yes | the folder a new bridge starts in: a real folder inside `roots`; a relative path is relative to the first root | |
| `default_permission_mode` | no | `default`, `acceptEdits` or `plan`; `bypassPermissions` is refused (use `/mode bypass`) | `default` |
| `backend` | yes | `coda`, or `echo` (the [echo test agent](#19-echo-backend)) | |
| `echo.answerTimeoutS` | no | with `echo`: how long an `ask` or `pick` card waits, 10 to 3600 seconds | `300` |
| `coda.exe` | with `coda` | the path to `coda.exe` (a bare name is looked up on `PATH`) | |
| `coda.args` | no | extra arguments after `serve`, as a list | `[]` |
| `control` | no | serve the local [control channel](#14-control-channel-for-other-tools) (the tray's status and actions, `pause`, `resume`); `false` turns it off, and the bridge works as without it (the tray then cannot see or reach it) | `true` |
| `peek` | no | keep the owner conversation's last 50 messages in memory, masked, for the tray's [chat peek](#chat-peek) (the control channel's `peek`); `false` keeps nothing and answers `peek_disabled` | `true` |
| `calls` | no | voice calls with the owner: `answer` (`auto`), `language` (`en-US`), `adapter` (the [voice adapter](#voice-adapter) doing the speech: `{"spawn": {…}}` or `{"connect": {…}}`; without it no calls are advertised), `notify_by_call` ([every field](#the-calls-section)). `stt`, `tts` and `vad` are no longer accepted ([migrating](#migrating-from-callsstt-and-callstts)) | absent: no calls (not advertised) |

Unknown fields are refused (a typo is an error, not ignored), and a UTF-8 byte order mark
as Notepad writes it is fine. In JSON, double every backslash. `push-agent-bridge status`
prints what is wrong with the configuration.

### 3. Create the key

```powershell
C:\Tools\push-agent-bridge.exe keygen
```

```text
Key storage: Microsoft Platform Crypto Provider (TPM)
Thumbprint:  <43 base64url characters>
```

`keygen` opens the key named `key.name`, or creates it: a persisted, non-exportable P-256
key in the Microsoft Platform Crypto Provider (TPM). Where the TPM provider is missing or
cannot create a P-256 key (`NTE_DEVICE_NOT_FOUND`, `NTE_PROVIDER_DLL_FAIL`,
`NTE_NOT_SUPPORTED`, `NTE_BAD_ALGID`), it creates the key in the Microsoft Software Key
Storage Provider instead (protected by the user's DPAPI keys, still not exportable), and
`Key storage` says so. Any other failure is reported (exit 3) rather than replaced by a new
key, so an existing identity is never silently swapped. Running `keygen` again prints the
same key; `push-agent-bridge thumbprint` prints only the thumbprint.

### 4. Create the agent and compare the thumbprint

As the owner, create the agent on the agents page of your app, or call `POST /v1/agents`
with your device access token and `{"handle":"coda","display_name":"Coda"}`
([details](agent-key-sign-in.md#enroll-and-operate-an-agent)). The answer holds a one-time
`enrollment_code` that works once, for ten minutes.

### 5. Enroll

```powershell
C:\Tools\push-agent-bridge.exe enroll --code <code>
```

```text
Enrolled as <user id>; device <device id>. Key thumbprint <thumbprint>.
```

`enroll` signs a fresh assertion with the key, redeems the code, registers this machine as a
device and stores the agent's user id and device id in `state.json`. Then check that
`GET /v1/agents` (or the agents page) shows the same `key_thumbprint` as `keygen` printed.
If they differ, delete the agent and start again.

### 6. Install the logon task

Either install the Scheduled Task below, or use the [tray app](#13-the-tray-app-push-agent-tray)
instead: it starts the bridges at logon itself, shows their state and restarts them. Never
both for the same folder: the tray detects a task that runs one of its folders, does not
start that bridge itself, and offers to switch over ([details](#scheduled-tasks-and-the-switch-over)).

```powershell
& .\crates\push-bridge\scripts\install.ps1 -Exe C:\Tools\push-agent-bridge.exe
```

The script (Windows PowerShell 5.1 or PowerShell 7) refuses to run without `config.json`,
then registers a Scheduled Task for the current user:

- trigger: at logon of the current user; runs as that user, interactive, without elevation;
- action: `%SystemRoot%\System32\conhost.exe --headless "<Exe>" --dir "<Dir>" run`, so no
  console window shows (where `conhost.exe` is missing the task runs the program directly
  and its window shows);
- settings: restart every minute after a failure, up to 999 times; no time limit; runs on
  batteries; a second start while one runs is ignored.

It starts the task at once unless `-NoStart`. Parameters: `-Exe` (required), `-Dir`
(default `%LOCALAPPDATA%\push-messenger\coda-bridge`), `-TaskName` (default
`push-agent-bridge`), `-Force` (replace a task of that name), `-NoStart`.

The bridge then signs in, starts Coda and answers the owner's messages. The log is
`<Dir>\logs\bridge.log`.

`scripts\uninstall.ps1` stops and removes the task (`-TaskName`, `-Dir`). `-DeleteKey` also
deletes the key named `-KeyName` (default: `key.name` from `config.json`) from both Windows
providers, or the `agent-key.p8` file with `software-file`; the agent must then be enrolled
again with a new key. `-DeleteState` removes the folder. Both ask first unless `-Force`.

### Several agents on one machine

Each agent is one instance with its own folder, key, task and agent account:

```powershell
$dir = "$env:LOCALAPPDATA\push-messenger\coda-bridge-docs"
# config.json in $dir with "key": {"name": "push-messenger-coda-docs"} and its own roots
C:\Tools\push-agent-bridge.exe --dir $dir keygen
C:\Tools\push-agent-bridge.exe --dir $dir enroll --code <code of the second agent>
& .\crates\push-bridge\scripts\install.ps1 -Exe C:\Tools\push-agent-bridge.exe -Dir $dir -TaskName push-agent-bridge-docs
```

Two instances must not share a `key.name` (they would be the same agent) or a folder (they
would share the state).

## 5. Command line

```text
push-agent-bridge [--dir <folder>] <command>
```

| Command | Does |
|---|---|
| `keygen` | creates the key if needed; prints its storage and thumbprint |
| `thumbprint` | prints the thumbprint of the existing key |
| `enroll --code <code>` | enrolls with the owner's one-time code and registers this device |
| `run` | runs the bridge until Ctrl-C (what the logon task runs) |
| `status` | prints the configuration path (and what is wrong with it), the key and its thumbprint, the agent user, the device, the folder, the sessions per folder, voice (`off (no calls section)`, `off (no calls.adapter)`, the voice adapter's state such as `adapter ready (…)`, or configured in a build without voice; [details](#build-with-voice)) and the log's path; no secrets |
| `pause [--reason <text>] [--drain-timeout <seconds>]` | asks the running bridge (through its [control channel](#14-control-channel-for-other-tools)) to [pause Coda for an upgrade](#15-upgrade-mode): Coda finishes its work (600 s at most by default), saves its state and is stopped; prints "Coda is paused; its program is released." once its program can be replaced. The bridge starts Coda again with its state once the program file changed, and holds the owner's texts meanwhile |
| `resume` | asks the running bridge to start the paused Coda now (its program must be usable) and prints "Coda is resuming."; a pause that is still draining is cancelled instead: "The pause was cancelled; Coda keeps running." |
| `help`, `--help`, `-h` | prints the usage |

`--dir` may come anywhere; `--dir=<folder>`, `--code=<code>`, `--reason=<text>` and
`--drain-timeout=<seconds>` work too. `keygen`, `thumbprint` and `status` read
`config.json` without validating it; `enroll` and `run` validate it first. `enroll` and
`run` write the log; the others print only. `pause` and `resume` find the bridge by its
folder (the `--dir`), name the backend as the bridge's status does ("The agent" while the
bridge is still signing in), and fail with exit code 1 when no bridge runs for the folder
("The bridge in <dir> is not running (no control channel)."), when it does not answer in
time ("The bridge did not answer in time."; `pause` waits the drain timeout plus 150 s,
`resume` 30 s), or when the bridge refuses (its message and code, such as "Coda is not
paused (not_paused)", `exe_unavailable`, or `unavailable` while it signs in).

| Exit code | Meaning |
|---|---|
| 0 | done (`run`: stopped by Ctrl-C or by a `shutdown` on the control channel, such as the tray's Stop) |
| 1 | failed: a bad command line, an unreadable or invalid `config.json` or `state.json`, a wrong, expired or spent enrollment code (`enrollment_invalid`), another refused sign-in or enrollment (for example `identity_in_use`, `rate_limited`, the network), the account is not an owned agent, the device was removed, or the bridge's data was opened elsewhere |
| 2 | not set up: no key (`run keygen first`), not enrolled (`run push-agent-bridge enroll --code <code>`), or the server does not accept the key (`token_invalid`: enroll again) |
| 3 | the key provider failed: the message carries its code, such as `NTE_BAD_KEYSET (0x80090016)`, never key material |

## 6. Behaviour

**Sign-in.** `run` signs in with a fresh assertion for the device in `state.json`, connects,
reads its owner from its own profile and opens the direct conversation with them. When the
server signs the device out (for example after `PUSH_AUTH_AGENT_KEY_MAX_SESSION_HOURS`), the
bridge signs in again with a new assertion, at most once every 30 s. The SDK reconnects by
itself and resends unsent replies; after a reconnect the bridge sends its activity again
and fetches missed background messages. The SDK's database is in memory, so replies not
yet sent when the bridge itself stops are lost.

**Sessions and folders.** The bridge runs one Coda session at a time. It stores the last
session id of each folder (by the folder's real path) and the current folder. At start, and
after Coda restarts, it resumes the stored session of the current folder; a session Coda
does not know there (-32002) is replaced by a new one. Session ids never move between
folders. A stored folder that is no longer inside `roots` falls back to `default_folder`.

**Turns.** A turn starts when Coda reports activity and ends at `event/turnEnded`, whether
the owner's prompt started it or the engine did (after a background task finished). A turn
is shown as **reply messages** (untitled): the first is sent at once, and the open one is
edited at most every 2 s while something changed, plus once at the end. A turn has several
replies when its text passes 4,000 characters or a [card](#7-cards) opens in it:

| Part | Content |
|---|---|
| `thinking` | the latest 4,000 characters of the thinking, in the first reply of a segment only (a segment is the turn up to the first card, or between two cards, or after the last) |
| `steps` | at most 50 tool calls; when full, the oldest finished step makes room. Each is labelled `<tool> <target>` (the path, command, query, pattern, URL or description from the call's input), masked, one line, at most 80 characters; a subagent's tool is `<task>: <tool> <target>`. Status `running`, then `done` or `failed`, with the duration as the detail; `pending` in a reply a card ended while the step ran |
| `text` | the assistant's text. Past 4,000 characters the reply is final and the text continues in a new reply (running steps move on with it) |

**Cards end a reply.** When a request opens a card (a permission, a question, a plan
approval, or the `/mode bypass` or `/yolo` confirmation), the reply in progress gets a
final edit and is not edited again; a step still running then (usually the one waiting for
approval) shows as `pending` in it. The card follows. The turn's next output after the
answer (thinking, a step, text, an error) starts a new reply, posted after the owner's
answer; the step that waited appears in it again and finishes there (`done` or `failed`,
with its duration). Requests that open with no output between them post their cards one
after the other. When the turn ends with no output after a card, no empty reply is posted;
only the activity goes idle. So the chat reads in the order things happened: the request,
the answer, then what the agent did next.

**Markdown.** Coda writes Markdown, so its replies are `agent.reply@2`: the same as
`agent.reply@1`, but clients render `text` as [safe Markdown](rich-messages.md#markdown)
(thinking and step details stay plain). The Markdown is sent as Coda wrote it, masked
first (also inside code spans and code blocks); the fallback is the text without its
markers (`**`, `__`, `~~`, backticks and fence lines), at most 2,000 characters. A page
never ends inside a code block: a page that would closes the fence, and the next page (or
the next reply after a card) opens it again with the same fence and language. A page whose
Markdown the server would refuse (raw HTML, an image, a link that is not http or https) is
sent as `agent.reply@1`, shown as typed. A backend that writes plain text
(`Backend::text_format()` is `TextFormat::Plain`, the default) keeps `agent.reply@1`, so
its text is never formatted by mistake (`2 * 3 * 4` stays as typed). The bridge's own
messages (command results, notes such as "No answer; skipped") are always
`agent.reply@1`.

An interrupted turn ends its text with "(stopped)" and marks running steps failed. If Coda
exits mid-turn, the text ends with "(Coda stopped; restarting)". Coda's `event/error`
becomes a line in the reply (or a reply of its own between turns). A body over the 32 KiB
RICH limit drops the thinking first, then shortens the fallback, then drops the oldest
steps.

**Background work.** Each `notify_user` message (`event/agentMessage`) becomes its own
reply titled with its label (`agent.reply@2`, as Markdown, like a turn's text). Its cursor and Coda's engine instance are stored;
after a messenger reconnect or a bridge restart against the same engine, the bridge fetches
what it missed with `session/pendingMessages` (and says "Some background messages were
lost." when Coda no longer has them). A background task's completion
(`event/taskCompleted`) becomes a step "Task <description> completed|failed|stopped" in the
running reply, or, between turns, a short reply with the first line of its report (masked,
at most 200 characters).

**Activity** ([live activity](activity-presence.md)) in the owner's conversation:

| State | Label | When |
|---|---|---|
| `working` | the running tool as `<tool> <target>` (at most 60 characters), else `thinking` | a turn runs |
| `waiting` | `approval needed` (permission, `/mode bypass`, `/yolo`), `question`, `plan approval` | a card is open (the oldest one) |
| `idle` | none | otherwise (sent once) |

Working and waiting are sent when they change and again every 15 s.

**The owner's text** in the direct conversation:

1. A `/` command runs ([commands](#8-commands)).
2. Otherwise, with a request open, the text answers the oldest open request
   ([typed answers](#7-cards)).
3. Otherwise, while a turn runs, it goes to `session/steer`. When the steer is refused with
   `noActiveTurn` or `turnEnding`, the text waits for the turn to end and is then sent as a
   prompt; another refusal is answered "Coda did not take it: <reason>".
4. Otherwise it is sent with `session/prompt`. If Coda answers busy, the bridge steers as
   above. With no turn to wait for (Coda can report a turn's end just before it is free
   again), it retries after 0.5 s, 1 s, 2 s, 4 s, 8 s… for at most 30 s in all, then
   replies "Coda stayed busy; this was not delivered, send it again:" with the first 500
   characters of the text.

Texts that wait are sent together as one prompt, separated by blank lines. Texts also wait
while `/compact` or `/fork` runs. Other JSON-RPC errors are answered with a short reply
"<method> failed (<code>): <message>".

**Coda stops.** When `coda serve` exits or its stream breaks, the reply in progress ends
with "(Coda stopped; restarting)", open requests are declined ("Declined: Coda
stopped."), and Coda is started again after 1 s, then 2 s, 4 s, … up to 60 s between
attempts (the backoff starts over once Coda has run for 60 s). After a restart the bridge
resumes the session and replies "Coda restarted. Undelivered background messages and
in-memory schedules were lost." A start that fails is reported once ("… ; retrying.") and
retried with the same backoff. When Coda stops (or fails to start) while its program file
(`coda.exe`) is missing or in use, as during an upgrade, the bridge says "(Coda stopped
and its program is missing or in use; waiting for it to come back)" and waits for the file
instead of spending restarts ([upgrade mode](#15-upgrade-mode)). The owner can also
restart Coda at once from the tray (Restart Coda): "(Coda is restarting: the owner
asked)", open requests "Declined: Coda restarted.", then "Coda restarted by the owner."
with what was lost; the chat connection is untouched.

**Stopping the bridge.** On Ctrl-C or a `shutdown` request (the tray's Stop), a running
turn ends with "(The bridge stopped.)", open requests are declined and the owner hears
"Declined: the bridge was stopped." once, the activity goes idle and Coda is stopped. The
device stays enrolled: the next `run` signs in again with a fresh assertion.

**Strangers.** Text and cards from anyone but the owner, in any conversation, get the plain
text "This agent is private." at most once per sender per hour; nothing they send reaches
Coda, and their answers to the owner's cards are ignored.

## 7. Cards

Every request from Coda becomes a card in the owner's conversation. The reply so far gets
its final state first (a running step shows as `pending`) and is not edited again; what
the turn does after the answer goes into a new reply below the answer
([turns](#6-behaviour)). Several requests in a row post their cards one after the other. A
card waits **10 minutes**.

| Coda request | Card | Answer back to Coda |
|---|---|---|
| `request/permission` | `agent.approval@1`: title "Allow <tool>?", details "Coda wants to use <tool> in <folder>.", preview = the masked `inputPreview` (at most 4,000 characters) | Approve → `{allow: true}`. Reject, typed text other than `yes`/`y`, or no answer → `{allow: false}` |
| `request/question`, single choice, 1 to 10 options | `agent.question@1` | the chosen label → `{answer}` |
| `request/question`, multi-select, 1 to 10 options | `agent.choose@1` | the chosen labels in option order, joined with ", " |
| `request/question` with no options or more than 10 | no card: a reply titled "Question" with the numbered options | typed text only |
| `request/planApproval` | `agent.plan-approval@1`, title "Approve the plan?", the masked plan. A plan over 4,000 characters is cut with "(The plan is cut here; the full text follows.)" and follows in full as replies titled "Plan (1/N)" | Approve → `{approve: true}`; Reject, any typed text, or no answer → `{approve: false}` |

**Typed answers** (the oldest open request): a permission or `/mode bypass` (`/yolo`) takes `yes` or
`y` (any case) as approve and anything else as reject; a plan takes any text as reject. A
question takes an option's label (any case) or its number; a multi-select also takes
several, separated by commas. Other text is the answer when Coda allows free text (or there
are no options); otherwise the bridge replies "Pick one of the options" (or "Type an
answer" for an empty text) and the request stays open. An answer that would be empty
declines the request (-32800).

**Answer checks.** The server checks an answer's form and values, but not who answers. The
bridge keeps its open cards in push-sdk's `CardBook` and accepts an answer only when it
comes from the owner, in the owner's conversation, names an open card, holds 1 to the
allowed number of distinct values the card offered, and arrives before the deadline. Any
other answer is logged (`answer_refused`) and ignored; the card stays open.

**Deadlines.** When 10 minutes pass, a permission gets `{allow: false}`, a plan
`{approve: false}`, a question error -32800, and `/mode bypass` leaves approvals on; the
bridge replies "No answer; skipped".

**Closed early.** Interrupting a turn, `/new`, `/cd`, `/resume` and a Coda exit close the
open requests as declined and say so once, e.g. "Declined: the turn was stopped." If the server refuses a card (`SendFailed`, or a send that fails at once), the
request is declined and the bridge replies "Could not show the card (<code>); the request
was declined."

## 8. Commands

Only the owner's commands run. A command needing a capability the backend lacks (`/new`,
`/cd`, `/sessions` and `/resume` need `Sessions`; `/mode` and `/yolo` need
`PermissionModes`; the others their own) answers "Not supported by <name>.", and `/help`
lists only the commands the backend supports. Arguments are checked; a bad one is answered with the usage
line (`Usage: /effort <low|medium|high|xhigh|max|auto>`). Command names are case-sensitive:
`/NEW` goes to Coda as a prompt. Results come back as masked `agent.reply@1` text (plain, never Markdown).
While Coda is [paused for an upgrade](#15-upgrade-mode), only `/status` and `/help` run;
every other command (Coda's own included) is answered "Coda is paused for an upgrade; try
again when it is back."

| Command | Effect |
|---|---|
| `/new` | a new session in the current folder; the permission mode returns to `default_permission_mode`. During a turn it interrupts the turn first and starts the session when the turn ends |
| `/cd <path>` | switches to a real folder (symlinks, junctions and `..` resolved; relative to the current folder; quotes allowed) inside `roots`, restarting `coda serve --cwd <path>` and resuming that folder's stored session or starting a new one. Refused outside the roots ("Cannot switch to …: outside the allowed roots. Allowed roots: …"), for a missing path or a file. `/mode bypass` does not carry over. Not during a turn |
| `/stop` | `session/interrupt`; "Nothing is running." when idle |
| `/status` | session id, folder, model, permission mode (`bypassPermissions (approvals are OFF until /new)` when bypassed), busy or idle, running tasks, Coda's version |
| `/compact` | `session/compact` (when idle; texts wait meanwhile) |
| `/model [id]` | without an id `session/models`: the models and the current one; with an id `session/setModel` |
| `/effort <low\|medium\|high\|xhigh\|max\|auto>` | `session/setEffort` |
| `/mode <default\|acceptEdits\|plan>` | `session/setPermissionMode` |
| `/mode bypass` | posts the approval card "Turn off approvals?" first; Approve → `bypassPermissions` ("Approvals are off until /new."). Reject or a timeout leave approvals on. `/new` resets it (and a bridge restart, `/cd` or `/resume` does too); `/status` shows it |
| `/yolo` | the same as `/mode bypass` (the name Coda's terminal uses): the same card first, the same reset and the same `/status` line. It takes no arguments; `/yolo <anything>` is answered with the usage line `Usage: /yolo` |
| `/goal [text]` | `session/setGoal`; without text, clears the goal |
| `/sessions` | `session/listSessions` for the current folder (up to 20) |
| `/resume <id>` | resumes that session of the current folder (`initialize {sessionId}`); "No session <id> in this folder." otherwise. Not during a turn |
| `/fork` | `session/fork` (when idle) and switches to the fork |
| `/tasks` | `session/listTasks {statusFilter: "active"}` |
| `/task stop <id>` | `task/stop` |
| `/btw <question>` | `session/askAside`: the answer comes back as a reply and stays out of the conversation's history |
| `/schedule list` | `session/scheduleList` |
| `/schedule add <every\|at\|cron> <when> <prompt>` | `session/scheduleCreate`; `<when>` is one word or `"quoted"` (`every 1h`, `at 2026-10-29T09:00`, `at "tomorrow 9am"`); a cron spec is five fields or quoted |
| `/schedule rm <id>` | `session/scheduleDelete` |
| `/skills` | `skills/list` |
| `/help` | lists the commands |
| any other `/…` | sent to Coda as a prompt (Coda's own slash commands work) |

Every `/schedule` reply ends with "Schedules live in Coda's memory and are lost when Coda
restarts."

## 9. Security model

| Threat | Mitigation |
|---|---|
| Someone else controls the agent | Only text from `owner_user_id` (from the agent's own profile) in the owner's direct conversation reaches Coda. Only the owner's card answers count, checked by the bridge with `CardBook` (sender, card, offered values, deadline), since the server does not check who answers |
| Key theft | A non-exportable P-256 key in the TPM (or the software provider, encrypted with the user's DPAPI keys); signing happens inside the provider and only the public key leaves it. Each assertion lives 120 s, has a new `jti` and is accepted once by auth-svc. `software-file` keys are plain files: development only |
| Secrets leaving the machine | Everything sent is masked first (below) and capped. **Message history is stored on the server unencrypted**: bodies, cards and answers are plaintext to the server and its operators, and stay in the history |
| Approval bypass from a stolen messenger session | `/mode bypass` (and `/yolo`) needs a confirmation card; `default_permission_mode` cannot be `bypassPermissions`; `/new`, `/cd` and a bridge restart reset it; `/status` shows it |
| Folder escape | `/cd` and `default_folder` accept only real paths (after symlinks, junctions and `..`) inside a root, compared by path component (`C:\src2` is not inside `C:\src`) |
| Command injection | Coda gets text over JSON-RPC, never through a shell; `coda serve` is started with its arguments as a list |
| Credentials in Coda's hands | The bridge never sets `CODA_SERVE_API_KEY` (an LLM credential in Coda's engine) and removes it from Coda's environment; stdio needs no key |
| Logs | Names, codes, counts and durations only: never message bodies, previews, plans, typed answers, tokens or assertions |
| Secrets at rest | Tokens stay in memory; the SDK's database is in memory; `state.json` holds ids only |
| Another local user drives the bridge | The [control channel](#access-rules) is a local pipe whose DACL admits only the user and SYSTEM, and each side checks the other's user SID (Unix: a 0600 socket in a 0700 folder and a peer uid check); it carries states, counts, names and error codes only, except the [chat peek](#chat-peek) (the owner conversation's last messages, masked, in memory only; `"peek": false` turns it off); `"control": false` turns the channel off |
| Someone else talks to the agent by voice | Only the owner's calls are answered (the server refuses non-contacts first), one at a time; a transcript is never a `/` command |

**Voice calls.** The call's audio is encrypted end to end between the owner's app and the
bridge. The call's audio and the text of each spoken sentence (masked) go to the
configured voice adapter, a local process of the same user; where it sends them (a local
engine, a cloud speech service) is up to the adapter. The adapter gets no credentials,
tokens or history, and a spawned adapter's environment has none of the bridge's secrets.
The bridge stores no audio; transcripts exist only as the masked `agent.transcript@1`
messages it posts (stored on the server like every message), and the logs carry no audio
or text. Details: [privacy](#privacy-what-the-adapter-receives).

**Masking.** Previews, plans, step labels, thinking, text, command results, background
messages and questions are masked before they leave the machine. Each match becomes
`«masked»`:

- `-----BEGIN … KEY-----` blocks (to their `-----END … KEY-----`, or to the end of the text);
- `Bearer <token>` and JWTs (`eyJ…`, including an empty signature);
- connection-string secrets: `AccountKey=`, `SharedAccessKey=`, `SharedAccessSignature=`,
  and `sig=` in SAS URLs;
- `name=value`, `name: value`, `name := value`, `"name": "value"`, `'name'='value'` and
  escaped `\"name\":\"value\"` pairs, and `--name value` flags, whose name contains `key`,
  `secret`, `token`, `password`, `passwd` or `pwd` (any case; quoted values are masked up to
  their closing quote);
- hex runs of 40 or more characters;
- base64 or base64url runs of 40 or more characters that look random: they mix upper case,
  lower case and digits, or end with `=` padding, or contain `+` or a 40-character hex run.

Previews, plans and the text of each reply are capped at 4,000 characters, step labels at 80,
titles at 120, activity labels at 60. Masking happens before a cut, so a cut never hides
the length that marks a secret.

Masking is a safety net, not a guarantee:

- **False positives** (accepted): any identifier containing a secret word is a secret's
  name (`monkey=banana`, `keyboard: logitech`), and a full 40-character Git commit id is
  masked.
- **False negatives**: a secret shorter than 40 characters without a telling name (a
  password in prose, `-p hunter2`, a short API key in a URL path); a long secret of only
  lower case and digits (or only upper case and digits), which looks like a name; a
  base64 run that looks like a path (at least two `/`, every segment made of words);
  secret words in other languages; secrets split across lines or across Coda's deltas in
  ways the patterns do not join. Do not let the agent print secrets, and remember that what
  it does print is stored on the server.

## 10. Files

All in the bridge's folder: `%LOCALAPPDATA%\push-messenger\coda-bridge` by default, or
`--dir`. (Elsewhere than Windows, for development, the default is
`$XDG_STATE_HOME/push-messenger/coda-bridge`, by default under `~/.local/state`.)

| File | Content |
|---|---|
| `config.json` | the configuration ([fields](#2-write-configjson)); written by you |
| `state.json` | written by the bridge (atomically): `device_id` and `registration_id` (the SDK's device and pending registration), `enrolled_user_id`, `folder` (the current folder), `sessions` (folder → session id), `agent_message_cursor` and `engine_instance` (background messages already shown). No tokens, no keys |
| `logs\bridge.log` | JSON lines (`tracing`), appended; at start a log over 10 MB is renamed to `bridge.log.1` (replacing it). Level from `PUSH_BRIDGE_LOG` (`tracing` filter syntax, e.g. `debug`), default `info` |
| `logs\voice-adapter.log` | only with a spawned [voice adapter](#voice-adapter) and `stderr` `log`: the adapter's stderr, appended; at each (re)start a log over 5 MiB is renamed to `voice-adapter.log.1` (replacing it). What it holds is up to the adapter |
| `agent-key.p8` | only with `"storage": "software-file"`: the private key as a plain PKCS#8 file |

With `"storage": "windows"` the key is in the user's CNG key store under `key.name`, not in
the folder. Log events include `started`, `signed_in`, `bridge_started`, `turn_started`,
`turn_ended`, `request_opened`, `card_answered`, `answer_refused`, `card_expired`,
`stranger_refused`, `command` (its name only), `backend_exited`,
`backend_restart_scheduled` (`delay_ms`), `backend_start_failed`, `session_not_found`,
`sign_in_failed`, `signed_out`, `send_failed` and `exit` (`code`). The control channel
and upgrade mode add `control_listening` (`instance`), `control_unavailable` (`reason` or
`kind`), `control_request` and `control_action` (the method), `control_peer_refused`,
`pause_started` (`drain_ms`), `backend_prepared`, `prepare_unsupported`, `pause_failed`,
`backend_released`, `program_changed`, `backend_resuming` (`restore`),
`pause_cancelled`, `backend_program_unusable` and `backend_restart_by_owner`. Voice calls
add the events listed in [privacy](#privacy-what-the-adapter-receives).

The tray keeps `tray.json` outside the bridge folders
([location](#trayjson)).

## 11. Troubleshooting

| Symptom | Cause and fix |
|---|---|
| Exit 2, "This agent is not enrolled: run `push-agent-bridge enroll --code <code>`…" | `state.json` has no `enrolled_user_id`: enroll with a new code. A new `--dir` starts unenrolled |
| Exit 2, "The server does not accept this agent's key (token_invalid)…" | No live agent is bound to this key: it was never enrolled, the agent was deleted, `key.name` names another key, or `audience` does not match `PUSH_AUTH_AGENT_KEY_AUDIENCE` (also check the clock: assertions live 120 s with 60 s of skew). Compare `thumbprint` with `GET /v1/agents`; enroll again with a new code |
| Exit 2, "…: run `push-agent-bridge keygen` first." | No key named `key.name` (or no `agent-key.p8`). Create it; if the agent was enrolled with a key that is gone, delete the agent and enroll a new one (key rotation is not supported) |
| Exit 1, "The enrollment code is wrong, expired or already used." | `enrollment_invalid`: codes work once, for ten minutes. Create a new one |
| Exit 1, "Sign-in failed: … identity_in_use" | The key is already bound to a live agent: it is enrolled, so just `run` (or install the task) |
| Exit 3, "The key provider failed: NTE_…" | CNG refused; the code says why, for example `NTE_DEVICE_NOT_READY` or `NTE_PERM` (TPM busy, locked out by too many failures, or policy), `NTE_BAD_KEYSET`/`NTE_NOT_FOUND` (the key was deleted), `NTE_SILENT_CONTEXT` (the provider wanted a UI). Check `Get-Tpm`, wait for a TPM lockout to end, run as the user who created the key (keys are per user) |
| `Key storage: Microsoft Software Key Storage Provider` | The TPM is missing, disabled or cannot create P-256 keys. The key still cannot be exported. To move to the TPM later, fix the TPM, delete the agent and the key (`uninstall.ps1 -DeleteKey`), and enroll again |
| "(Coda stopped and its program is missing or in use; waiting for it to come back)" | `coda.exe` is not at the path in `coda.exe` (or a bare name is not on the user's `PATH`), or an installer holds it. The bridge waits (state paused, phase `waiting_for_file`) and starts Coda once the file is there and unchanged for 2 s; fix `config.json` if the path is wrong. `push-agent-bridge resume` starts it at once when the file is usable |
| "spawn failed (spawn): cannot start Coda: …; retrying." | The file is there but cannot be started. The bridge keeps retrying with backoff meanwhile |
| "initialize failed (…)" or Coda restarting again and again | Coda starts but fails: its stderr is discarded, so run `coda serve --cwd <folder>` by hand in that folder to see why (sign-in, model configuration, MCP servers). Restarts wait 1 s, 2 s, … up to 60 s |
| "Coda stayed busy; this was not delivered, send it again" | Coda reported busy for 30 s with no turn to wait for (a compaction or maintenance). Send the text again; `/status` shows the state |
| "Wait until the turn ends." | `/cd`, `/resume`, `/compact` and `/fork` need an idle agent: `/stop` first, or wait |
| A new session after moving a folder | Sessions live in `<folder>\.coda\sessions` and belong to their folder: a moved or renamed folder is a new folder, and an unknown stored session is replaced by a new one. `/resume <id>` works only for a session Coda finds in the current folder |
| "Not supported by Coda." | The backend lacks that capability |
| "Could not show the card (…)" | The server refused the card: usually a built-in revoked by an admin, or a message-svc older than `agent.choose@1` and `agent.plan-approval@1` |
| No answer, nothing in the log | Is the task running (`Get-ScheduledTask push-agent-bridge`, `Get-ScheduledTaskInfo`)? Run `push-agent-bridge status`, then `push-agent-bridge run` in a console to see errors |
| "The bridge stopped: this device was removed" (exit 1) | The device was revoked (the agent was deleted, or its devices revoked); the task restarts it and sign-in tells what is wrong |
| A console window shows | The task runs without `conhost --headless` (no `conhost.exe`, or an old task): reinstall with `install.ps1 -Force`. Closing that window stops the bridge until the task restarts it |
| More detail | Set `PUSH_BRIDGE_LOG=debug` for the user and restart the task; the log never holds bodies or tokens |
| "The bridge in <dir> is not running (no control channel)." | No bridge serves that folder's control channel: it is not running, `--dir` names another folder, `"control": false`, or the bridge could not bind its pipe (the log says `control_unavailable`; `kind` `AddrInUse` means another bridge already serves that folder) |
| The tray shows an agent red with "Not set up: run keygen and enroll" | The bridge exited with code 2: run `keygen` and `enroll` for that folder, then Start it from the tray |
| The tray shows "stopped after 5 failures (exit N)" | The bridge exited 5 times within 10 minutes; Open log from its menu, fix the cause, then Start |
| The tray shows "running (Scheduled Task)" | The folder's Scheduled Task runs that bridge, not the tray. Stop asks it to shut down through the control channel; Quit leaves it running; the tray never restarts it; "Switch from Scheduled Task…" moves it under the tray |
| The tray shows "running (started outside the tray)" | The bridge answers but the tray did not start it and no Scheduled Task runs it: it was started in a console, by a script, or by an earlier tray process. Stop, Quit and restarts as for a Scheduled Task's bridge |
| The tray shows "stopped" and its history in the window lists "stopped outside the tray (exit 0)" | A bridge the tray started was stopped on purpose elsewhere: Ctrl-C, or `shutdown` from another tool (`push-agent-bridge` CLI or another control-channel client). The tray does not restart it and does not count it as a failure; Start runs it again |
| "Coda is paused for an upgrade; try again when it is back." | Upgrade mode: wait for the upgrade, or `push-agent-bridge resume` |
| The agent cannot be called with `calls.adapter`, or "Voice adapter stopped; reconnecting…" | `status` shows the adapter's state and the last reason; see [voice adapter troubleshooting](#voice-adapter) |

## 12. Live smoke test (opt-in)

Not run in CI: it needs a deployment with `agent-key`, a Windows machine with Coda and an
owner's app. Use a scratch root folder.

1. **Create an agent and enroll.** `keygen`, create the agent, compare the thumbprint,
   `enroll --code <code>`, `install.ps1`. The agent shows in the owner's contacts, and
   `status` shows the agent user and the device.
2. **Ask something harmless** ("What is in this folder?"). One reply appears, shows
   thinking and steps while it runs, ends with text, and the activity goes working → idle.
3. **Ask Coda to edit a scratch file**, then approve it from the "Allow …?" card. The
   activity shows `approval needed` while the card is open; the edit happens after
   Approve. (Optionally: let a card time out and see "No answer; skipped".)
4. **Run `/status`, `/model` and `/compact`.** Each answers with a reply; `/status` shows the
   session, folder, model, mode, `idle`, tasks and Coda's version.
5. **Check that a stranger gets the fixed reply.** From another account, message the
   agent: it answers "This agent is private." once, and nothing reaches Coda (the log shows
   `stranger_refused`).

**Voice.** The voice pipeline is tested end to end in CI (`bridge-voice`:
`crates/push-smoke/tests/bridge_voice.rs` and `bridge_voice_adapter.rs`, opt in locally
with `PUSH_BRIDGE_VOICE=1` and Docker): the owner's call over real media, a prompt, a
spoken reply, barge-in, a voice approval and a stranger refused, with the testkit's
tone-based voice adapter ([testing](#testing)). Real speech engines are tested in their
own adapters; to try one with the bridge, configure it in `calls.adapter` and call the
agent from the owner's app.

## 13. The tray app (push-agent-tray)

`push-agent-tray` is a small Tauri v2 app (Windows first; [Linux later](#16-linux-plan))
that supervises the bridge instances of the logged-on user: a tray icon (the app's glyph
with a badge that shows the most urgent agent's state), a slim native menu, one
management window and notifications. It replaces the Scheduled Task as the way the bridges
start at logon. Everything it decides lives in the crate `push-tray-core` (tested on every
platform); `apps/push-agent-tray` is the shell around it.

It runs once per user: a second launch exits and leaves the running one alone. At start it
reads [`tray.json`](#trayjson), looks for [Scheduled Tasks](#scheduled-tasks-and-the-switch-over)
that run its folders, and then, for every folder marked `autostart` that no task runs,
first connects to the folder's [control channel](#14-control-channel-for-other-tools): a
bridge that already answers is only shown (the tray did not start it), else the tray starts
`push-agent-bridge.exe --dir <folder> run` from its own folder, with no window and no
console input or output (the bridge logs to its folder). It keeps a control connection to
each bridge (retried every 0.5 s for 30 s after a start, then every 10 s), follows its
events, and asks for its status every 10 s.

### What it shows

The icon is the app's glyph, two linked chat bubbles (the owner and the agent): an
outlined bubble top left, a solid one overlapping it bottom right. It is drawn in one
colour that follows the taskbar: `#1b1b1b` on a light taskbar, `#ffffff` on a dark one
(Windows: `SystemUsesLightTheme` under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`, checked every 5 s).
It is rendered for its size (crisp at 16, 20, 24 and 32 px). The SVG source is
`crates/push-tray-core/assets/glyph.svg`; the app's own icon (the glyph in white on a
rounded blue tile, used for the exe and the window) is generated from it by the build
(`apps/push-agent-tray/build.rs` writes `icons/icon.ico` and `icons/icon.png`).

On the solid bubble's corner sits a badge, a dot in the colour of the most urgent agent
("worst wins": the first row below that applies to any agent), with a transparent ring
around it so it reads on any taskbar. With no agents there is no badge.

| Badge | Agent states (label) | From the bridge's state |
|---|---|---|
| red `#d13438` | "stopped", "failed (exit N), retrying in S s", "stopped after 5 failures (exit N)", "Not set up: run keygen and enroll" | none: the bridge is not running |
| grey `#8c8c8c` | an agent that is up (idle, working, waiting, paused, running) while the tray has no control connection to it | |
| amber `#e8a317` | "waiting for you (approval needed)", "… (question)", "… (plan approval)" | `waiting` with its detail |
| purple `#8a5cd1` | "paused for an upgrade" | `paused` |
| blue, pulsing `#3b7be8` | "starting", "stopping", "<backend> restarting" ("Coda restarting", "Echo restarting"), "disconnected, reconnecting" | `starting`, `stopping`, `restarting`, `reconnecting` |
| blue `#3b7be8` | "working" | `working` |
| green `#2e9d4f` | "connected, idle"; "running (Scheduled Task)"; "running (started outside the tray)" | `idle` (the second label when the tray did not start the process and the folder's Scheduled Task runs it, the third when no task runs it) |

The pulsing badge alternates two frames (full and dimmed, 0.5 s each) only while that
state is the worst; with "Reduce motion" (the window's setting, or the system's "reduce
motion" as the window reports it) it is a steady dot. A stopped agent is red, so stopping
one agent turns the badge red. The tooltip has a line
per agent, "`<name>: <label>`" (for example "Coda on Devbox (coda-devbox): waiting for
you (approval needed)"), cut at 127 characters with "…"; without agents it reads
"push-agent-tray" and "No agents". An agent's name is "`<display name> (<handle>)`" once
its bridge has signed in, else its folder's name.

### The menu

A left click on the icon opens the [window](#the-window); a right click opens the menu.
From the top: a line "tray.json: <error>" when [`tray.json`](#trayjson) could not be read;
one line per agent, "`<name>: <state label>`" with a small dot in its badge colour ("No
agents" when there are none), which opens the window on that agent; "Open…" (the window);
"Add agent folder…"; the check item "Start at logon"; "Switch from Scheduled Task…" when
[offered](#scheduled-tasks-and-the-switch-over); "Quit". The tray's own items have access
keys (`&` mnemonics) and the agent lines are reached with the arrow keys, so the menu
works without a mouse.

With "Show last agent line in the menu" on (the window's setting, off by default), an
agent's line shows the last line the agent wrote in its conversation instead of its
state, cut to 60 characters ("Coda: Tests pass; 3 files changed…"). This is the only
place outside the window where [chat peek](#chat-peek) text appears; never in the tooltip
or a notification.

While a [voice call](#18-voice-calls) runs or rings, an agent's line shows the call
instead of its state or last line: "`<name>: calling you`" (the agent rings you,
`notify_by_call`), "`<name>: your call is ringing`" (your call waits for you to confirm it
on another device, `calls.answer` `ask`) or "`<name>: on a call`" (connecting or in the
call). The call does not change the badge or the tooltip: the badge keeps the agent's
state colour.

### The window

One window, "push-agent-tray" (900 × 600 at first, resizable; its size and position are
kept in `tray.json`). A left click on the icon, "Open…", an agent's menu line and a second
launch of the tray show and focus it; closing it hides it (the tray keeps running).
Everything in it works with the keyboard (Tab, the arrow keys in the agent list and the
tabs).

**Left:** the agents, each with its name (display name and handle), its badge, its state
label (followed by the call in a few words, as in the menu, while one runs or rings) and
its folder; below them "Add agent folder…".

**Right:** the selected agent, in four tabs.

- **Overview:** the state with its badge and label; the actions; the call's buttons; the
  voice call, the voice adapter (with `calls.adapter`: "<name> <version> (ready: stt,
  tts, vad, barge-in)" with its capabilities, "…; cannot listen" without `stt`,
  "starting" or "backing off (<reason>)"), the folder, session, the backend's version
  ("Coda version", "Echo version"), uptime,
  running tasks, last error (the
  tray's own, else the bridge's error code) and last exit code; "Open folder"; the last 50
  states with their times, newest first. A refused action says so next to its button, not
  in a dialog.

  | Action | Does | Enabled when |
  |---|---|---|
  | Start | starts the bridge (or runs its Scheduled Task); starts the failure count over | stopped, failed, stopped after 5 failures, not set up |
  | Stop | [stops](#stop-and-quit) the bridge gracefully | running (any running state, also one the tray did not start) or failed (cancels the pending restart) |
  | Restart Coda (Restart <backend>) | `restart_backend`: the backend is stopped and started again at once; the chat stays connected | the control channel is connected (not while stopping) |
  | Reconnect | `reconnect`: the bridge signs in again and reconnects; Coda is untouched | the same |
  | Pause for upgrade | `pause_backend` with the reason "paused from the tray" and the default drain ([upgrade mode](#15-upgrade-mode)) | the same, and not paused |
  | Resume | `resume_backend` | the same, and paused |
  | Mute agent | `mute`: the agent's voice is muted in the call (the button reads "Unmute agent", `mute` with `muted: false`, while it is muted) | the control channel is connected (not while stopping) and a call is connecting or in progress (not while it rings) |
  | Hang up | `hangup`: hangs up the call, cancels a call ringing you, or declines your call waiting for confirmation | the control channel is connected (not while stopping) and a call runs or rings |

  The call reads "ringing (you are calling; confirm on your other device)", "calling
  you", "connecting", or "in a call — speaking" / "— listening", with " (muted)" while
  the agent's voice is muted; "no call" without one. The tray follows it from the
  bridge's `status` and `call_changed` events ([control channel](#events)); it forgets it
  when the connection is lost. A `no_call` refusal (the call ended meanwhile) shows next
  to the button like any other.

  An agent whose bridge exited with code 2 shows "Not set up: run keygen and enroll"; only
  Start is enabled.
- **Chat:** the [chat peek](#chat-peek): the last messages of the agent's conversation
  with its owner, newest last, each with its time, as plain text. "Open in messenger"
  opens `https://<host>/`, the host of `api_url` in the bridge's `config.json`, when that
  is an `https://` URL. The tab refreshes when the bridge says the peek changed, and at
  least every 2 s while it is shown; never while the window is hidden.
- **Logs:** a live tail of `<folder>\logs\bridge.log`: the last 500 lines, followed as the
  file grows (and read again when it is rotated). Each JSON line is shown as time, level,
  event and its other fields; "Raw" shows the lines as written. A level filter (Info: all;
  Warn: warnings and errors; Error), a search box, "Copy" (the shown lines) and "Open in
  editor". The log holds no message content.
- **Settings:** "Autostart" (start this agent with the tray); "Show chat peek" (on by
  default) and "Messages kept" (10–50, default 20); the bridge's `config.json` path,
  read-only, and "Open config" (opens it in its editor: the tray itself never writes
  `config.json`); "Remove agent from tray" (asks again inline; the folder leaves
  `tray.json` and the tray stops the bridge if it runs it; the agent stays enrolled and
  its folder is untouched).

**Bottom:** "Start at logon", "Reduce motion", "Show last agent line in the menu",
"Switch from Scheduled Task…" (when offered) and "Quit".

Wherever the tray names the agent's backend ("Restart Coda", "Coda restarting", "Coda
version") it uses the name the bridge reports in its `status` (`backend.name`: "Coda",
"Echo"); before the first status it says "backend".

### Chat peek

The bridge keeps, in memory only, the last 50 messages of the conversation with its owner:
what the owner sent (texts and card answers), what the bridge sent (reply text, card
titles with their choices, the bridge's own notes) and one line per tool step ("✓
run_command dotnet test"). Each entry is masked exactly as it was sent (the bridge masks
before sending; the owner's texts are masked the same way) and cut to 300 characters.
Nothing is written to disk, and the entries are gone when the bridge exits. The window
asks for the last 20 (the agent's "Messages kept") with the control channel's
[`peek`](#requests) request.

**Privacy.** Peek is the one place where (masked) message text crosses the control
channel; everything else on it stays content-free. It is local and per user like the rest
of the channel ([access rules](#access-rules)). It can be turned off per agent in the
bridge (`"peek": false` in `config.json`: the bridge keeps nothing and answers
`peek_disabled`) and in the tray ("Show chat peek" off: the tray does not ask). Logs,
notifications and the tooltip never show it.
### Start at logon

The check item writes the value `push-agent-tray` under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (with `reg.exe`), with the data
`"<path to push-agent-tray.exe>" --autostart`; clearing it deletes the value. Per user, no
admin rights. `--autostart` changes nothing: whenever the tray starts, it starts the
folders marked `autostart`. Toggling the item starts or stops no bridge. On Linux the same
item writes the XDG autostart entry `$XDG_CONFIG_HOME/autostart/push-agent-tray.desktop`
(default `~/.config/autostart`), see the [Linux plan](#16-linux-plan).

### tray.json

The folders the tray supervises: `%LOCALAPPDATA%\push-messenger\tray.json` on Windows,
`$XDG_CONFIG_HOME/push-messenger/tray.json` (default `~/.config/push-messenger/tray.json`)
on Linux.

```json
{
  "version": 1,
  "instances": [
    { "dir": "C:\\Users\\me\\AppData\\Local\\push-messenger\\coda-bridge", "autostart": true },
    { "dir": "C:\\Users\\me\\AppData\\Local\\push-messenger\\coda-bridge-docs", "autostart": false }
  ]
}
```

| Field | Meaning | Default |
|---|---|---|
| `version` | must be `1` | `1` |
| `instances[].dir` | a bridge folder (the bridge's `--dir`) | |
| `instances[].autostart` | start it when the tray starts | `true` |
| `instances[].show_peek` | the window's Chat tab asks this bridge for its [chat peek](#chat-peek) | `true` |
| `instances[].peek_messages` | how many peek entries the Chat tab shows (10–50) | `20` |
| `menu_peek` | an agent's menu line shows its last line ("Show last agent line in the menu") | `false` |
| `reduce_motion` | the badge does not pulse | `false` |
| `window` | `{x, y, width, height}`: where the window was last closed | |

Fields at their default are not written. The window's settings change this file (only
when it could be read).

Unknown fields are refused; a byte order mark is fine; a missing file means no folders.
Two entries for the same folder (by [instance id](#endpoints): case, `/` and a trailing
separator do not matter on Windows) count once.

- **The default folder** `%LOCALAPPDATA%\push-messenger\coda-bridge` is added by itself
  (with `autostart`) when it holds a `config.json` and is not listed yet, and the file is
  saved.
- **"Add agent folder…"** opens a folder picker. The folder must hold `config.json` ("<folder>
  is not a bridge folder: it has no config.json"); one already listed is refused ("… is
  already in the tray."). It is added with `autostart` and started (unless a Scheduled Task
  runs it).
- The tray writes the file atomically (`tray.json.tmp`, then renamed over it). The
  window's Settings turn an agent's `autostart` on or off and remove it from the tray.
- A file that cannot be read or parsed (or has another `version`) is never overwritten:
  the menu's first line shows the error, the default folder is still supervised, and "Add
  agent folder…" and the switch-over refuse until the file is fixed (or deleted) and the
  tray restarted.

### Scheduled Tasks and the switch-over

At start the tray (Windows) lists the Scheduled Tasks with `Get-ScheduledTask` (through
`powershell.exe`, no window) and finds those whose action runs `push-agent-bridge(.exe)`,
directly or as `conhost.exe --headless "<exe>"` (what `install.ps1` registers), with the
command `run` and `--dir "<folder>"`, `--dir=<folder>` or no `--dir` (the default folder).
A task is matched to a `tray.json` folder by instance id. The tray never starts a bridge
that a task runs (never both): it does not start it at logon, it only connects to it (an
idle one shows "running (Scheduled Task)"), Start runs the task (`Start-ScheduledTask`) and
Stop asks the bridge to shut down.

While such a task exists, the menu offers "Switch from Scheduled Task…". After the
confirmation ("Stop and remove the Scheduled Task(s) <names>, start push-agent-tray at
logon, and run these agents under the tray instead?", Switch or Cancel) the tray:

1. stops each task (`Stop-ScheduledTask`; this is not the graceful Stop, so a bridge that
   ends with its task does not decline the cards open at that moment) and removes it (`Unregister-ScheduledTask
   -Confirm:$false`), each addressed by its Task Scheduler folder and name;
2. turns its own "Start at logon" on;
3. marks each switched folder `autostart` in `tray.json` (adding it if absent; folders added
   meanwhile are kept) and saves it;
4. starts each switched bridge under the tray once the task's bridge is gone. Stopping a
   task may leave the bridge it ran alive (under `conhost`): while the folder's control
   channel still answers, the tray sends it `shutdown` (so it declines its open cards) and
   looks again every 0.5 s for up to 20 s. A bridge still there after that is not joined
   by a second one: the tray does not start that folder and says so ("Not started under the
   tray: the bridge in <folder> is still running 20 s after its Scheduled Task was stopped
   …"); stop it, then use Start.

It stops at the first failure and says so: a task that could not be stopped or removed
leaves "Start at logon" off and `tray.json` unchanged, so a folder never ends up started by
both. Tasks for folders not in `tray.json` are not touched.

### Restarts and failures

When a bridge the tray started exits without a Stop, the tray starts it again after a
delay, except after exit code 0 or 2:

| Rule | Value |
|---|---|
| Delay | 1 s, then 2 s, 4 s, … up to 60 s; a run of 60 s or more starts the delay over; shown as "failed (exit N), retrying in S s" |
| Giving up | at the 5th exit within 10 minutes: "stopped after 5 failures (exit N)" and the notification "<name> stopped after 5 failures (exit N). Open its log from the tray window." |
| Exit code 2 (no key, not enrolled, key not accepted) | never retried: "Not set up: run keygen and enroll" and the notification "<name> is not set up: run keygen and enroll." |
| Exit code 0 the tray did not ask for | the bridge exits 0 only when stopped on purpose (Ctrl-C, or `shutdown` from another tool such as the CLI): "stopped", with "stopped outside the tray (exit 0)" in the window's history; never restarted and not counted |
| A bridge that cannot be started (no `push-agent-bridge.exe` next to the tray) | counts as an exit |
| Start | forgets the exits and starts the delay over |

Coda's own restarts happen inside the bridge (its [backoff](#6-behaviour)) and show as
"Coda restarting"; they do not count here. A bridge the tray did not start is never
restarted by it; when it goes away it shows "stopped".

### Stop and Quit

**Stop** sends `shutdown` over the control channel. The bridge ends a running turn with
"(The bridge stopped.)", declines the open requests and tells the owner once "Declined: the
bridge was stopped.", stops Coda and exits with 0; the device stays enrolled and the next
Start signs in again. For a bridge the tray started, the tray waits up to 5 s for the
answer and then 20 s for the exit before it kills the process. When the tray has no live
control connection to it (it lost the connection, say), Stop first opens one (for up to
3 s) and sends `shutdown` over it; only when that fails is the process killed, if it is
still alive (no graceful decline then). A stopped bridge is not restarted.

**Quit** stops (as above, all at once) only the bridges this tray process started, waits
for them to exit, and leaves the others running (a Scheduled Task's, one started in a
console, or one an earlier tray process started: the bridges are not tied to the tray's
life, so after a tray crash they keep running and the next tray only shows them, as
"running (started outside the tray)").

### What it never does

The tray never runs `keygen` or `enroll`, never touches keys, tokens, `config.json` or
`state.json` (it only reads `config.json`'s `api_url` for "Open in messenger" and opens the
file in its editor when asked), and never sees message content except the masked [chat
peek](#chat-peek) in the window's Chat tab (and, when turned on, the menu's last agent
line): the control channel's other requests and events carry states, counts, names and
error codes only, and so do the tooltip and the notifications. It changes Scheduled Tasks
only in a switch-over the user confirmed.

### Security

- No network listener: the tray is only a client of the bridges' local, per-user [control
  channels](#access-rules).
- The window loads only the page bundled into the exe at build time (`dist/`), no
  remote content, under this CSP (`tauri.conf.json`):

  ```text
  default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src ipc: http://ipc.localhost; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'
  ```

- The window (`main`) is granted no IPC permissions at all (`capabilities/default.json`
  has `"permissions": []`: no core or plugin command such as tray, menu, window or event),
  and the global Tauri API is off. It reaches the app only through the app's own commands
  (`window_view`, `agent_view`, `act`, `open_folder`, `open_config`, `open_log`,
  `log_tail`, `peek`, `open_messenger`, `set_agent_setting`, `set_global_setting`,
  `remove_agent`, `add_folder`, `set_start_at_logon`, `switch_over`, `quit`,
  `report_motion`), which answer only the `main` window. The page renders everything with
  `textContent`, never HTML or Markdown, and shows no links. `peek` answers only while the
  window is visible.
- "Open in messenger" opens only `https://<host>/` built from the bridge's own
  `config.json` (`api_url`, which must be `https://`, with a plain host name), never a URL
  from the page or from a message.
- Helper programs run without a shell, with full paths under `%SystemRoot%` where they are
  Windows tools (`reg.exe`, `powershell.exe` with `-EncodedCommand`, `explorer.exe`), and
  without windows.

### Install (per user, no admin rights)

Copy `push-agent-tray.exe` next to `push-agent-bridge.exe` (for example into `C:\Tools`):
the tray starts the bridge from its own folder. Run it, check that the agents show, and
turn on "Start at logon". Remove the bridges' Scheduled Tasks first, or let the tray's
switch-over do it. `tauri.conf.json` also describes an MSI (`cargo tauri build`, with the
Tauri CLI); it installs per machine (admin rights) and `push-agent-bridge.exe` would still
have to be placed next to the installed exe, so the portable exe is the supported way.

### Build

```powershell
cargo build --manifest-path apps/push-agent-tray/Cargo.toml --release
```

The exe is `apps/push-agent-tray/target/release/push-agent-tray.exe`. The app is its own
Cargo workspace (excluded from the root one, so Tauri stays out of the root lockfile and
MSRV); no Tauri CLI and no Node are needed. It needs the WebView2 runtime, part of Windows
10 and 11, for the window. CI builds it on Windows (job `tray-windows`). The build
renders the app icon (`icons/icon.ico`, `icons/icon.png`) from the glyph's SVG; these two
files are generated, not checked in.

**Signed builds:** not yet. The exe is unsigned, so SmartScreen may warn on first run;
code signing is a follow-up.

## 14. Control channel (for other tools)

Every `push-agent-bridge run` serves a local control channel: the tray uses it, `pause` and
`resume` use it, and other tools of the same user may too. It is newline-delimited JSON
over a named pipe (Windows) or a Unix socket.

### Endpoints

| Platform | Endpoint |
|---|---|
| Windows | the named pipe `\\.\pipe\push-agent-bridge-<instance id>` |
| Linux and other Unix | the socket `$XDG_RUNTIME_DIR/push-agent-bridge/<instance id>.sock`; without an absolute `$XDG_RUNTIME_DIR` there is no control channel (the log says `control_unavailable`, `reason` `no_runtime_dir`) |

The **instance id** names a bridge by its folder: the first 8 bytes (16 lowercase hex
characters) of the SHA-256 of the normalized folder path, as UTF-8. Normalized means: the
real path (`std::fs::canonicalize`: symlinks and junctions resolved; for a folder that does
not exist the absolute path, else the path as given), as a string; on Windows with `/`
replaced by `\`, a leading `\\?\` removed (`\\?\UNC\` becomes `\\`) and in lowercase; on
Unix with its case kept; without trailing separators. So
`C:\Users\Me\AppData\Local\push-messenger\coda-bridge\` and
`c:/users/me/appdata/local/push-messenger/coda-bridge` are the same instance. The bridge
logs its id (`control_listening`, `instance`).

### Access rules

- **Windows.** The pipe is created as the first instance of its name (a pipe that already
  exists, another bridge on the same folder or a squatter, makes the bind fail), rejects
  remote clients, and has the protected DACL `D:P(A;;GA;;;<user SID>)(A;;GA;;;SY)`: the
  user and SYSTEM only. The bridge checks each client's process user SID
  (`GetNamedPipeClientProcessId`) and drops any that is not the user's or SYSTEM's (logged
  as `control_peer_refused`); a client checks the server the same way
  (`GetNamedPipeServerProcessId`) and refuses a server of another user.
- **Unix.** The socket's folder `$XDG_RUNTIME_DIR/push-agent-bridge` is created with mode
  0700; an existing one must be a folder owned by the owner of `$XDG_RUNTIME_DIR` with no
  group or other bits, else the bind fails. A stale socket file is replaced only when
  nobody answers on it; the socket is made 0600 and removed when the bridge exits. Both
  sides check the peer's uid (the socket's peer credentials) against the owner of
  `$XDG_RUNTIME_DIR`.

When the bind fails, the bridge logs `control_unavailable` with the error's kind and runs
on without a control channel.

**Turning it off:** `"control": false` in `config.json`. The bridge then opens no endpoint
and behaves as before; the tray cannot see it (an autostart folder would be started again
by the tray, so do not list such a folder in `tray.json`), and `pause`/`resume` say it is
not running.

### Lines

One JSON object per line, UTF-8, ended by `\n` (a `\r` before it is ignored, blank lines
are skipped), at most 64 KiB (65,536 bytes) without the newline: a longer line closes the
connection. Right after connecting, the bridge sends `hello` and then a `state_changed`
with the current state. A client sends requests; the bridge answers each with a response
carrying the request's `id`, and sends events as things happen. A client may send several
requests without waiting: `status` is answered at once, the others when done, each with
its `id`.

```text
request   {"id": <u64>, "method": "<method>", "params": {…}}       params optional (absent, null or {})
response  {"id": <u64>, "ok": true,  "result": {…}}
          {"id": <u64>, "ok": false, "error": {"code": "<code>", "message": "<text>"}}
event     {"event": "<name>", …fields, "at": <unix ms>}
```

A line that is not a request gets `bad_request` (with the request's `id` when it could be
read, else `id` 0) and the connection stays; an unknown method gets `unknown_method`.

### Requests

| Method | Params | Result | Errors (`code`) |
|---|---|---|---|
| `status` | none | the [status object](#the-status-object) | `internal` (cannot happen in practice) |
| `restart_backend` | none | `{"restarted": true}`: Coda was stopped and started again at once (no backoff); what ran is cut and declined ("Declined: Coda restarted."); the chat connection is untouched | `paused` (resume instead), `start_failed` (the start failed; the bridge retries with its backoff), `unavailable`, `timeout` |
| `reconnect` | none | `{"reconnected": true}`: signed in again with a fresh assertion and reconnected; Coda is untouched. While the bridge waits to sign in again, it signs in at once | the SDK's error code (such as `network`), `timeout` |
| `shutdown` | none | `{"stopping": true}`, then the bridge [stops](#stop-and-quit) and exits with 0 | `timeout` |
| `pause_backend` | `reason` (string, trimmed, at most 100 characters kept), `drainTimeoutMs` (default 600,000) | at the release: `{"state": "paused", "prepared": <bool>, "drained": <bool>, "interruptedTasks": <n>}`; already released: at once `{"state": "paused", "phase": "<phase>"}` ([upgrade mode](#15-upgrade-mode)) | `prepare_failed`, `cancelled` (a `resume_backend` cancelled the drain), `stopping` (the bridge stopped meanwhile), `unavailable`, `timeout` |
| `resume_backend` | none | `{"resumed": true}`; a draining pause is cancelled: `{"resumed": true, "cancelled": true}` | `not_paused`, `exe_unavailable` ("coda.exe is missing or in use"), `unavailable`, `timeout` |
| `peek` | `limit` (default 20, clamped to 1–50) | `{"entries": [...]}`: the [chat peek](#chat-peek)'s last `limit` entries, oldest first (newest last), at once | `peek_disabled` (`"peek": false` in the bridge's `config.json`) |
| `call_state` | none | `{"call": <call> \| null}`: the [voice call](#18-voice-calls) as in the status object's `call`, at once | none |
| `mute` | `muted` (default `true`; `false` unmutes) | `{"muted": <bool>}`: the agent's voice in the running call is muted (or unmuted) | `no_call` ("no call is running"), `unavailable`, `timeout` |
| `hangup` | none | `{"hungUp": true}`: the running call is hung up, a call ringing the owner is cancelled, or the owner's call waiting for confirmation is declined | `no_call`, `unavailable`, `timeout` |

A peek entry is `{"time": <unix ms>, "from": "owner"|"agent", "kind": "text"|"reply"|"card"|"answer"|"step"|"note", "text": "…", "card_state": "open"|"answered"|"declined"|"expired"}`
(`card_state` only for cards). `text` is masked as it was sent and at most 300
characters. Kinds: `text` the owner's message, `answer` the owner's card answer (the
chosen choices), `reply` the agent's reply (with its title, "Title: text"), `card` a card's
title or question and its choices ("Allow run_command? [Approve] [Reject]"), `step` one
line per tool step ("✓ run_command dotnet test", "✗ …" failed, "… …" running), `note` the
bridge's own words ("(Coda stopped; restarting)"). An edited reply (a streaming one)
replaces its entries in place. `peek` is answered while the bridge signs in too (with what
it has).

`unavailable` ("the bridge is signing in") answers every action but `shutdown` and
`reconnect` while the bridge signs in or waits to sign in again. `timeout` is the hub's own
limit: 30 s for an action, and for `pause_backend` its `drainTimeoutMs` (10 minutes when it
does not say) plus 2 minutes; the action may still finish afterwards (`status` shows
how it ends). The drain is cut at its timeout and Coda's save takes at most 60 s more, so
a pause normally answers within that limit.

### Events

| Event | Fields | When |
|---|---|---|
| `hello` | `protocol` (1), `bridge` (its version), `pid`, `instance` | first line of every connection |
| `state_changed` | `state`, `detail` (only when there is one) | the state or its detail changed; second line of every connection |
| `turn_started` | `source`: `prompt` or `engine` | a turn starts |
| `turn_ended` | `interrupted` | a turn ends |
| `request_opened` | `kind`: `permission`, `question`, `plan` or `bypass`; `open`: the open requests now | a card opens |
| `request_closed` | `kind`; `outcome`: `answered`, `declined` or `expired`; `open` | a card is answered, declined or expires |
| `error` | `code` (Coda's JSON-RPC code or the bridge's, such as `-32603`, `exited`, `spawn`, `timeout`), `method` (such as `session/prompt`, `initialize`) | a Coda call failed |
| `peek_changed` | none | the [chat peek](#chat-peek) changed (only with `"peek": true`); ask `peek` for the entries |
| `call_changed` | `call` (as in the status object; absent when the call ended) | a voice call started, changed its phase or flags, or ended; the status already shows it |
| `adapter_changed` | `adapter` (as in the status object, without `metrics`) | the voice adapter's state changed (starting, ready, backing off); only with `calls.adapter` |

States, from the most important: `stopping`; `paused` (detail "upgrading Coda", or
"waiting for Coda's program"); `reconnecting` (the messenger connection is down, or the
bridge waits to sign in again); `restarting` (Coda is not running); `waiting` (a card is
open; detail "approval needed", "question" or "plan approval"); `working` (a turn,
`/compact` or `/fork` runs; no detail, since tool labels can hold paths and commands);
`idle`. `starting` until the bridge has signed in and started Coda. A slow client that
falls more than 256 events behind skips the missed ones.

### The status object

Field names are camelCase; absent values are `null`.

| Field | Meaning |
|---|---|
| `protocol` | 1 |
| `instance` | the instance id |
| `pid` | the bridge's process id |
| `bridgeVersion` | the bridge's version |
| `state`, `detail` | as in `state_changed` |
| `agent` | `{displayName, handle}` once signed in |
| `connection` | `connected`, `connecting`, `reconnecting`, `offline`, `signed_out`, `device_removed` or `opened_elsewhere` |
| `folder` | Coda's current folder |
| `session` | Coda's session id |
| `backend` | `{name, version, running, runningTasks}`: "Coda", Coda's `serverInfo` as it reports it, whether it runs, its running background tasks |
| `openRequests` | open cards |
| `heldTexts` | the owner's texts held during a pause (a count) |
| `pause` | `{phase, reason, since}` while paused: `phase` is `draining`, `released`, `waiting_for_file` or `resuming`; `since` in Unix ms |
| `lastError` | `{code, at}` of the last failed Coda call |
| `startedAt` | when the bridge started, Unix ms |
| `call` | the [voice call](#18-voice-calls) while one runs or rings, else `null` (absent from older bridges): `{phase, direction, speaking, listening, muted, since}`. `phase` is `ringing` (the owner's call waits for the owner's confirmation, or the bridge's call rings the owner), `connecting` or `in_call`; `direction` is `incoming` (the owner called the agent) or `outgoing` (the agent calls the owner, `notify_by_call`); `speaking`: the agent's voice plays; `listening`: the owner speaks; `muted`: the agent's voice is muted (`mute`); `since`: when the call entered its phase, Unix ms |
| `adapter` | the voice adapter, only with `calls.adapter` (absent otherwise): `{state, name, version, capabilities, reason, retryAt, metrics}`. `state` is `off`, `starting`, `handshaking`, `ready` or `backoff`; while ready, `name`, `version` and `capabilities` (`stt`, `tts`, `vad`, `barge_in`, `partials`) from its hello; while backing off, `reason` (content-free) and `retryAt` (Unix ms); `metrics`: `{calls, framesToAdapter, framesFromAdapter, droppedToAdapter, droppedFromAdapter, restarts, speaks, finalToSpeak, speakToAudio}`, the latencies as `{lastMs, avgMs, maxMs}` (final transcript → `speak`, `speak` → its first audio) |

### Limits

| | |
|---|---|
| Line length | 64 KiB; longer closes the connection |
| Connections | 8 at once per bridge; more are closed at once (no `hello`) |
| Action timeout | 30 s (`pause_backend`: its drain timeout, default 10 minutes, plus 2 minutes) |
| Events a connection may fall behind | 256 (then skipped) |

### Example session

`→` is the client, `←` the bridge (times shortened).

```text
← {"event":"hello","protocol":1,"bridge":"0.1.0","pid":4242,"instance":"2612a5f44f292114","at":1793660033911}
← {"event":"state_changed","state":"idle","at":1793660033911}
→ {"id":1,"method":"status"}
← {"id":1,"ok":true,"result":{"agent":{"displayName":"Coda on Devbox","handle":"coda-devbox"},"backend":{"name":"Coda","runningTasks":0,"running":true,"version":"0.1.176"},"bridgeVersion":"0.1.0","connection":"connected","detail":null,"folder":"C:\\src\\app","heldTexts":0,"instance":"2612a5f44f292114","lastError":null,"openRequests":0,"pause":null,"pid":4242,"protocol":1,"session":"0b9c…","startedAt":1793659000000,"state":"idle"}}
← {"event":"turn_started","source":"prompt","at":1793660040100}
← {"event":"state_changed","state":"working","at":1793660040100}
← {"event":"request_opened","kind":"permission","open":1,"at":1793660043500}
← {"event":"state_changed","state":"waiting","detail":"approval needed","at":1793660043500}
← {"event":"request_closed","kind":"permission","outcome":"answered","open":0,"at":1793660051200}
← {"event":"state_changed","state":"working","at":1793660051200}
← {"event":"turn_ended","interrupted":false,"at":1793660060000}
← {"event":"state_changed","state":"idle","at":1793660060000}
→ {"id":2,"method":"resume_backend"}
← {"id":2,"ok":false,"error":{"code":"not_paused","message":"Coda is not paused"}}
```

### From PowerShell

The instance id of a folder (an existing folder reached through a junction or symlink
needs its real path; `Get-ChildItem \\.\pipe\ | Where-Object Name -like
'push-agent-bridge-*'` lists the bridges' pipes), then `status`:

```powershell
$dir  = "$env:LOCALAPPDATA\push-messenger\coda-bridge"
$full = [IO.Path]::GetFullPath($dir).TrimEnd('\').ToLowerInvariant()
$hash = [Security.Cryptography.SHA256]::Create().ComputeHash([Text.Encoding]::UTF8.GetBytes($full))
$id   = -join ($hash[0..7] | ForEach-Object { $_.ToString('x2') })
$pipe = [IO.Pipes.NamedPipeClientStream]::new('.', "push-agent-bridge-$id", [IO.Pipes.PipeDirection]::InOut)
$pipe.Connect(5000)
$reader = [IO.StreamReader]::new($pipe)
$writer = [IO.StreamWriter]::new($pipe)
$writer.AutoFlush = $true
$reader.ReadLine()                                  # hello
$writer.WriteLine('{"id":1,"method":"status"}')
do { $line = $reader.ReadLine() } until ($line -like '{"id":1,*')
$line | ConvertFrom-Json
$pipe.Dispose()
```

### Guarantees

- Content-free: no message bodies, previews, plans, answers, tool targets or labels,
  thinking, tokens, keys or enrollment codes, in events, in the status or in the bridge's
  logs about the channel; only states, counts, names (the agent's display name and handle,
  the backend's name and version, the folder, the session id), the voice call's phase,
  direction, flags and time (`call`, `call_changed`, `call_state`, `mute`, `hangup`) and
  error codes. The one
  exception is the `peek` result: the owner conversation's last messages, masked as sent
  and cut to 300 characters, kept in memory only, and only when `config.json` has
  `"peek": true` (the default). Tests check this: the protocol's types classify every
  request, event and status field, and a bridge test sends a marker through a whole
  conversation and finds it on the channel only in `peek`.
- Per user: only the same user (or SYSTEM) can connect, and clients can check that the
  bridge is the user's too.
- Every request gets exactly one response with its `id`; `hello` and the current state
  always come first.
- The protocol version is 1; a later version may add fields, events and methods, which
  clients should ignore.

## 15. Upgrade mode

Upgrade mode lets Coda's program be replaced (`dotnet tool update -g coda`, say) while the
bridge keeps the chat: Coda finishes its work and saves its state, is stopped so that its
files are free, and starts again with its state once the program changed. The owner's
texts meanwhile are held and sent afterwards. It is started with `pause_backend` (the
tray's "Pause Coda for upgrade", or `push-agent-bridge pause`).

### The steps

1. **Pause.** `pause_backend {reason?, drainTimeoutMs?}` (default 600,000 ms; the reason
   trimmed and cut to 100 characters). The state becomes `paused` (detail "upgrading
   Coda", phase `draining`), and the owner's activity shows working "Paused: upgrading
   Coda" (waiting while a card is open). From now on:
   - text that would go to Coda is held; the first held text of a pause is answered
     "Coda is being upgraded; your messages will be sent when it is back." (once);
   - commands other than `/status` and `/help` (Coda's own included) are answered "Coda is
     paused for an upgrade; try again when it is back.";
   - answers to open cards, by card or typed, still count;
   - a prompt Coda refuses with -32016 (restart pending) is held too.

   A second `pause_backend` while draining waits for the same release; once released it
   is answered at once with `{"state": "paused", "phase": "<phase>"}`. When Coda is not
   running at that moment (it is restarting), it is released at once.
2. **Drain.** The bridge calls Coda's `session/prepareRestart {drainTimeoutMs, reason?,
   deliveredCursor?}` (`deliveredCursor` is the last background message the bridge
   showed, so Coda does not deliver it again after the restore; it waits up to the drain
   timeout plus 60 s). Meanwhile turns, cards and answers go on.
   Coda's answer releases it. A Coda without that method (-32601) gets the
   [fallback](#older-coda-fallback). Any other error ends the pause: the callers get
   `prepare_failed` ("session/prepareRestart failed (<code>)"), the owner hears "Pausing
   Coda failed: session/prepareRestart failed (<code>): <message>", and the held texts are
   sent. If Coda exits while draining, it counts as released.
3. **Release.** The bridge fingerprints Coda's program file (its path, length,
   modification time and, on Windows, its file version), notes Coda's version and whether
   approvals were off (`/mode bypass`), ends a turn still running with "(Coda paused for
   an upgrade)", declines Coda's open requests ("Declined: Coda is being upgraded."), and
   stops Coda, which frees its files; a pending restart is cancelled. The `pause_backend`
   callers get `{"state": "paused", "prepared": <prepareRestart succeeded>, "drained": <the
   work finished first>, "interruptedTasks": <n>}`; `pause` prints "Coda is paused; its
   program is released." The phase is `released`.
4. **Watch.** Every 2 s the bridge looks at the program file (`coda.exe` as configured, a
   bare name through `PATH` and `PATHEXT`). It resumes once the file is usable (it exists,
   can be opened for reading and is not empty), differs from the fingerprint at the
   release, and looked the same at two looks in a row (so a file still being written is
   not taken). A missing or locked file just waits. `resume_backend` resumes at once
   instead.
5. **Resume.** The phase is `resuming`. The bridge starts the folder's stored session,
   with `restore: true` when `prepareRestart` succeeded, in `default_permission_mode` when
   approvals were off (else in the current mode). Then it tells the owner how Coda came
   back:
   - with Coda's `restored` (applied): "Coda upgraded 0.1.176 → 0.1.177: 2 schedules, 1
     message and the model/goal restored; 1 interrupted task and 1 interrupted request
     reported to Coda." The head is "Coda upgraded <before> → <after>" when both versions
     are known and differ, "Coda upgraded to <after>" when only the new one is, else "Coda
     restarted"; the items are "N schedule(s)", "N message(s)" and "the <settings joined by
     />" (without `bypassPermissions`, `permissionMode` and anything in
     `settingsNotRestored`), joined "a, b and c" + " restored", or "nothing to restore";
     then "; N interrupted task(s) and N interrupted request(s) reported to Coda" (each
     when there are any); then ".". "Before" is `restored.fromVersion`, else the version
     Coda reported before the pause (else the program's file version then); "after" is
     `restored.toVersion`, else the version Coda reports now, or the new file version when
     it reports none or the same one. Versions taken from Coda's `serverInfo` lose a
     leading program name and a " (contract …)" suffix ("coda 0.1.177 (contract …)" reads
     "0.1.177"; "coda (contract …)" is no version);
   - with `restored.applied: false`: the head, that the state was not restored and why,
     then what was lost, e.g. "Coda upgraded 0.1.176 → 0.1.177; its saved state was not
     restored (expired). Undelivered background messages and in-memory schedules were
     lost." (reasons: `missing` "none was found", `unreadable` "it could not be read",
     `sessionMismatch` "it belongs to another session", `workingDirMismatch` "it belongs
     to another folder", `expired`, `consumeFailed` "it could not be taken",
     `unsupportedFormat` "its format is newer");
   - without `restored`: the head, then what was lost, e.g. "Coda upgraded 0.1.176 →
     0.1.177. Undelivered background messages and in-memory schedules were lost.";
   - when approvals were off before the pause, or `restored.settingsNotRestored` names
     `permissionMode: bypassPermissions` (once, however many say so): "Approvals were off
     before the upgrade; confirm to turn them off again." and a new "Turn off approvals?"
     card.

   Then the pause ends, the state is published, and the held texts go to Coda as one prompt
   (separated by blank lines). A failed start is reported once ("… ; retrying.") and retried
   with the normal backoff (or waits for the program, below); the start that finally works
   still restores, announces and asks for the confirmation.

### The Coda contract

As shipped in Coda 0.1.177 (coda-cli PR #214):

| Call | Bridge sends | Bridge reads |
|---|---|---|
| `session/prepareRestart` | `{"drainTimeoutMs": <ms>, "reason"?: "<text>", "deliveredCursor"?: <the last background message shown>}` | `drained` (default `true`), `interruptedTasks`, `interruptedRequests`, `suspendedTasks` (each a number or a list: its length; default 0; logged as counts); `ok`, `restartFile` are not used; `ok: false` is a refusal; error -32601: not supported ([fallback](#older-coda-fallback)) |
| any call while draining | | error -32016 "restart pending": a prompt is held (-32012, a stale epoch, is an ordinary refusal) |
| `event/lifecycle` | | `{"lifecycle": "draining"}` and the like: logged at debug, otherwise ignored |
| `initialize` | as always, plus `"restore": true` after a successful `prepareRestart` | `restored`: `applied` (default `true`), `ignoredReason`, `fromVersion`, `toVersion`, `schedules`, `messages`, `interruptedTasks`, `interruptedRequests`, `steering`, `tasksSuspended` (counts: a number or a list: its length; missing: 0), `settings` (a list of names, or of objects with `name`, or an object whose keys are the names), `settingsNotRestored` (a list of `{name, value?, reason?}` or of names); `preparedAt`, `reason` and `tasks` are not used; `restored: true` means restored with nothing to list; absent or anything else: no restore |
| `initialize` failing with -32017 | | the restart file is in a newer format: the bridge calls `initialize` again for the same session without `restore`, and announces it as `applied: false` with the reason `unsupportedFormat` |

Every field is optional and parsed leniently. `bypassPermissions` is never restored (Coda
lists it in `settingsNotRestored` as `permissionMode: bypassPermissions`): the bridge
starts Coda in the default mode and asks the owner to confirm with a new card, and never
names `bypassPermissions`, `permissionMode` or any setting not restored among the restored
settings. `session/getState` and `coda serve --restore` are not used.

### Older Coda (fallback)

When Coda answers `session/prepareRestart` with -32601, the bridge waits until no turn and
no `/compact` or `/fork` runs, or until the drain timeout, whichever comes first (a turn
still running then is cut with "(Coda paused for an upgrade)" and `drained` is `false`),
then releases Coda as above (`prepared: false`). The restart starts Coda without
`restore`, and the announcement lists what was lost: "Coda upgraded 0.1.176 → 0.1.177.
Undelivered background messages and in-memory schedules were lost." (or "Coda restarted.
…" when the version did not change or is unknown).

### Coda stops while its program is missing or locked

When Coda exits on its own, or a start fails, and its program file is missing or cannot be
opened (an installer that replaces it, or a wrong `coda.exe`), the bridge does not burn its
restarts: it says "(Coda stopped and its program is missing or in use; waiting for it to
come back)" (as the end of the running turn's reply, else as a reply), declines open
requests ("Declined: Coda stopped."), and pauses with phase `waiting_for_file` (detail
"waiting for Coda's program", reason "Coda's program is missing or in use"; the owner's
activity shows working "Waiting for Coda's program", not "Paused: upgrading Coda"). Held
texts and commands work as in a pause. Once the file is usable and looked the same twice, 2 s
apart, Coda starts again, without `restore` (nothing was saved), and the owner hears the
announcement with what was lost. `resume_backend` starts it at once when the file is
usable, else answers `exe_unavailable`.

### resume_backend

| Phase | `resume_backend` |
|---|---|
| not paused | error `not_paused` ("Coda is not paused") |
| `draining` | cancels the pause: Coda keeps running, the held texts are sent, waiting `pause_backend` callers get `cancelled`; answer `{"resumed": true, "cancelled": true}`. If Coda had already saved its state, its next start restores it |
| `released`, `waiting_for_file` | when the program file is usable (changed or not): starts Coda now ([step 5](#the-steps)), answer `{"resumed": true}`; else error `exe_unavailable` ("coda.exe is missing or in use") |
| `resuming` | `{"resumed": true}` |

`restart_backend` is refused while paused (`paused`): resume instead.

### From the command line

```text
push-agent-bridge [--dir <folder>] pause [--reason <text>] [--drain-timeout <seconds>]
push-agent-bridge [--dir <folder>] resume
```

| Command | Prints (exit 0) | Fails (exit 1) |
|---|---|---|
| `pause` | "Coda is paused; its program is released." once released (at once when it already was); it waits up to the drain timeout plus 150 s (the bridge answers `timeout` after the drain plus 120 s) | "The bridge in <dir> is not running (no control channel).", "The bridge did not answer in time.", or the bridge's refusal "<message> (<code>)" such as "session/prepareRestart failed (-32603) (prepare_failed)" |
| `resume` | "Coda is resuming.", or "The pause was cancelled; Coda keeps running." for a pause still draining; it waits up to 30 s | the same, such as "Coda is not paused (not_paused)" or "coda.exe is missing or in use (exe_unavailable)" |

"Coda" is the backend's name from the bridge's status ("The agent" while the bridge is
still signing in).

### Upgrading Coda from a script

```powershell
push-agent-bridge pause; dotnet tool update -g coda; push-agent-bridge resume
```

`pause` returns once Coda's files are free. `resume` is optional: the bridge sees the new
program file by itself (within a few seconds) and starts Coda; if it already did, `resume`
answers `not_paused` (exit 1), which is harmless here. With `--dir`, give it to both
commands.

## 16. Linux plan

The bridge, its control channel and `push-tray-core` already build and are tested on
Linux; the tray shell is built and tested on Windows only. The plan for Linux:

- **Tray:** the same Tauri app; its tray icon on Linux is a StatusNotifierItem through
  AppIndicator, so it needs `libayatana-appindicator3` (or `libappindicator3`) and a
  desktop that shows such items (GNOME needs the AppIndicator extension), plus WebKitGTK
  for the window. The glyph's colour should follow the panel's theme (dark until that is
  wired).
- **Start at logon:** the tray's "Start at logon" writes the XDG autostart entry
  `$XDG_CONFIG_HOME/autostart/push-agent-tray.desktop` (implemented in `push-tray-core`):

  ```ini
  [Desktop Entry]
  Type=Application
  Name=push-agent-tray
  Exec="/home/me/.local/bin/push-agent-tray" --autostart
  X-GNOME-Autostart-enabled=true
  ```

  Without the tray, a systemd user unit runs a bridge (exit code 2, not set up, is not
  retried):

  ```ini
  # ~/.config/systemd/user/push-agent-bridge.service
  [Unit]
  Description=push-agent-bridge (Coda)

  [Service]
  ExecStart=%h/.local/bin/push-agent-bridge --dir %h/.local/state/push-messenger/coda-bridge run
  Restart=on-failure
  RestartSec=5
  RestartPreventExitStatus=2

  [Install]
  WantedBy=default.target
  ```

  `systemctl --user enable --now push-agent-bridge`. Like a Scheduled Task, a unit and the
  tray must not both run the same folder; the tray does not detect units yet.
- **Key storage:** off Windows the bridge has only `"storage": "software-file"` (a plain
  key file, for development); a platform key store is to come.
- **Control channel:** the socket `$XDG_RUNTIME_DIR/push-agent-bridge/<id>.sock`
  ([access rules](#access-rules)). `$XDG_RUNTIME_DIR` is required (systemd's user session
  sets it); without it the bridge runs without a control channel and the tray cannot see
  it.
- **Default folders:** the bridge's `$XDG_STATE_HOME/push-messenger/coda-bridge` (default
  `~/.local/state/…`) and the tray's `$XDG_CONFIG_HOME/push-messenger/tray.json`.
- **Packaging:** an AppImage and a `.deb`, later.

## 17. Manual smoke test on Windows

Not run in CI. It needs two enrolled agents (section [4](#4-setup-on-windows), steps 1–5,
without the logon task: the second agent as in [several agents](#several-agents-on-one-machine)),
Coda, the owner's app, and `push-agent-tray.exe` next to `push-agent-bridge.exe`. Remove
any Scheduled Task of these folders first (`scripts\uninstall.ps1`, or the tray's
switch-over).

1. **Two agents show in the tray, each with its state.** Start `push-agent-tray.exe`. The
   default folder is added by itself; add the second with "Add agent folder…". Expected:
   the glyph in the taskbar's colour (switch Windows between light and dark mode: it
   follows within 5 s) with a green badge; two menu lines "<display name> (<handle>):
   connected, idle" with green dots; the tooltip has one line per agent; `tray.json` lists
   both folders. A left click opens the window with both agents; a second launch of the
   exe focuses it; closing and reopening it keeps its size and position.
2. **A card opening turns the badge to "waiting for you".** Ask one agent to edit a scratch
   file. While the turn runs the badge is blue ("working"); when the "Allow …?" card opens
   it turns amber, and the tooltip, the menu line and the window say "waiting for you
   (approval needed)". The Chat tab shows the request, the tool steps and the card "Allow
   …? [Approve] [Reject]" (open); approve: the card shows answered, the badge goes blue
   again, then green when the turn ends.
3. **Restart Coda keeps the chat connected.** Choose the agent's "Restart Coda" in the
   window. Expected in the chat: "(Coda is restarting: the owner asked)", then "Coda
   restarted by the owner." with what was lost; the badge pulses blue ("Coda restarting")
   for a moment (steady with "Reduce motion" checked), then green. The agent stays online,
   the next message is answered, and `/status` shows the same session.
4. **Reconnect signs in again.** Choose "Reconnect". Expected: "disconnected,
   reconnecting" for a moment, then "connected, idle"; the log shows `messenger_reconnect`
   and no `backend_exited`; Coda was not restarted (no "Coda restarted" in the chat, the
   same session in `/status`).
5. **Stop declines open cards.** Open a card as in step 2 and, without answering it,
   choose "Stop". Expected: the chat says "Declined: the bridge was stopped." (and the turn's
   reply ends with "(The bridge stopped.)"); answering the card now has no effect; the
   agent shows "stopped" (red icon) and is not restarted. "Start" brings it back,
   "connected, idle".
6. **Start at logon works after signing out and back in.** Check "Start at logon";
   `reg query HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v push-agent-tray` shows
   `"<path>\push-agent-tray.exe" --autostart`. Sign out of Windows and back in. Expected:
   the tray icon appears without a console window, both agents come back "connected,
   idle" (started by the tray: Quit stops them), and both answer a message.
7. **Logs, settings and the chat peek's switches.** In the Logs tab, the last lines show
   formatted and new ones appear as the agent works; "Warn" hides the info lines, the
   search filters, "Raw" shows the JSON, "Copy" copies, "Open in editor" opens the file.
   In Settings, change "Messages kept" to 10 and turn "Show chat peek" off: the Chat tab
   says the peek is off; restart the tray: the settings are kept. Set `"peek": false` in
   the bridge's `config.json` and restart that agent: the Chat tab says the bridge has
   the peek turned off. Turn "Show last agent line in the menu" on: the agent's menu line
   shows its last reply (60 characters at most).
8. **The keyboard is enough.** With the window focused, Tab through the agent list (Up
   and Down select), the tabs (Left and Right switch) and the buttons; open the tray menu
   with Win+B and the arrow keys, the context-menu key (or Shift+F10), and the access keys.

## 18. Voice calls

The owner can call the agent from their app ([voice calls](calls.md)) and talk to it: the
bridge answers, turns what the owner says into prompts, speaks the agent's replies, and
takes spoken answers to its cards. The speech itself (hearing when the owner speaks,
recognising it, synthesising the replies) runs in a separate **voice adapter** process
that the bridge starts or connects to ([voice adapter](#voice-adapter),
[protocol](voice-adapter-protocol.md)); the bridge carries the audio and keeps the
conversation. Design: the
[voice calls spec](superpowers/specs/2026-11-03-voice-calls-design.md) and its addendum
[2026-11-04-voice-adapter-design](superpowers/specs/2026-11-04-voice-adapter-design.md);
this guide describes what the code does.

### Who gets through

| Call | What happens |
|---|---|
| The owner calls, nothing else runs | answered at once (`calls.answer` `auto`, the default); the bridge says "Connected to <name> in <folder>." (the backend's name and the last part of the current folder), then the oldest open card's question, if any |
| The owner calls, `calls.answer` `ask` | a card asks first ([asking before answering](#asking-before-answering)); answered only when the owner picks "Answer" before the ring ends, then greeted as above |
| Anyone else calls | refused by the server when they are not the agent's contact (`NOT_CONTACT`); a call from anyone but the owner that reaches the bridge is declined |
| A second call while one runs (or is being answered, asked about or placed, or the bridge is stopping) | declined busy |
| No `calls` section, or no `calls.adapter` | calls are not advertised, so the owner's app sees the agent as unreachable (the server answers `UNREACHABLE`) |
| `calls.adapter`, and the [voice adapter](#voice-adapter) is not ready (or cannot listen) | the same: calls are advertised only while the adapter is ready with `stt` |
| A build without `--features voice` | calls are not advertised; a `calls` section only prints a warning at start (`voice_unavailable`) |

One call at a time. The owner ends it by hanging up; stopping the bridge hangs it up, and
so does the tray's **Hang up** ([call controls](#call-controls-in-the-tray)). The agent
can also call its owner ([the agent calls you](#the-agent-calls-you)).

### Build with voice

```powershell
cargo build --release -p push-bridge --features voice
```

The feature `voice` brings push-sdk's [media helper](calls.md#19-media-helper) (feature
`calls-media`: ICE, DTLS-SRTP and Opus natively) and nothing else; it needs **Rust 1.89**
and **CMake** with the MSVC build tools; the bridge without it keeps Rust 1.85. The bridge
has no speech engine of its own. Everything else of the pipeline (the
[voice adapter](#voice-adapter) client, spoken text, voice answers, the policies) builds
and is tested without the feature; without it no adapter is started and calls are not
advertised.

`push-agent-bridge status` prints a `Voice:` line:

| `Voice:` | Meaning |
|---|---|
| `off (no calls section)` | no `calls` in `config.json` |
| `off (no calls.adapter)` | a `calls` section without `adapter`: nothing does the speech, calls are not advertised |
| `configured, but this build has no voice (build with --features voice)` | rebuild with the feature |
| `adapter ready (<name> <version>; stt, tts, vad, barge-in)` | the running bridge's adapter is ready with these capabilities; calls are advertised (after 2 s ready, never during a call) |
| `adapter ready (…), but calls are not advertised: the adapter cannot listen (no stt)` | the adapter has no `stt` |
| `adapter starting`, `adapter starting (handshake)` | the bridge is starting or connecting to the adapter, or waits for its hello |
| `adapter backing off <n> s (<reason>)` | the adapter ended or could not be started or reached; the bridge tries again in `n` seconds |
| `adapter configured (the bridge is not running)` | `status` asks the running bridge; none runs |

With a running adapter a `Voice adapter:` line follows with its counters
([metrics](#voice-adapter)). A `config.json` that still has `calls.stt`, `calls.tts` or
`calls.vad` shows the error under `Configuration:` ([migrating](#migrating-from-callsstt-and-callstts)).

### The calls section

The `calls` section of `config.json`. Unknown fields are refused here too; `calls` is
validated (by `run`, `enroll` and `status`) whether or not the build has voice.

| Field | Meaning | Default |
|---|---|---|
| `calls.answer` | `auto`: the owner's call is answered at once. `ask`: a card asks the owner first ([asking before answering](#asking-before-answering)) | `auto` |
| `calls.language` | the language spoken, a BCP 47 tag; sent to the voice adapter in its hello and each `call_started` | `en-US` |
| `calls.adapter` | the [voice adapter](#voice-adapter) doing the speech: `{"spawn": {…}}` or `{"connect": {…}}`. Without it the bridge takes no calls and places none | |
| `calls.notify_by_call` | when the agent calls its owner ([the agent calls you](#the-agent-calls-you)); absent: never | |

```json
"calls": {
  "language": "en-US",
  "adapter": { "spawn": { "command": ["C:\\Tools\\voice\\voice-adapter.exe"] } }
}
```

**Validation messages** (`status` prints them; `run` and `enroll` exit 1 with "invalid
configuration: <message>"):

| Message | Fix |
|---|---|
| `calls.stt is no longer supported: speech runs in a voice adapter. Configure calls.adapter (docs/agent-bridge.md §18, docs/voice-adapter-protocol.md).` (also `calls.tts`, `calls.vad`) | remove the field and configure `calls.adapter` ([migrating](#migrating-from-callsstt-and-callstts)) |
| `calls.language must be a language tag such as "en-US"` | a tag like `en-US`, `de-DE`, `en` |
| `calls.notify_by_call.after_minutes must be 0 (off) to 8: a card expires after 10 minutes` | the call must ring and talk before the card's 10 minutes are over |
| `calls.notify_by_call.min_interval_minutes must be 10 to 1440` | at most one call per 10 minutes, or less often |
| `calls.notify_by_call.do_not_disturb[<i>] <why>: write "22:00-07:00", "mon-fri 22:00-07:00" or "sat,sun 00:00-24:00"` | `<why>` is `needs a time range`, `has a bad time "…"` (`HH:MM`, two digits each; `24:00` only as an end), `has an unknown day "…"` (`mon` … `sun`) or `is empty (its start and end are the same)` |
| `calls.notify_by_call needs calls.adapter: without it the bridge takes no calls and places none` | add `calls.adapter`, or set `after_minutes` 0 and `urgent` false |

The `calls.adapter` messages are in [voice adapter](#voice-adapter).

### Migrating from `calls.stt` and `calls.tts`

Earlier builds had speech providers of their own (`azure-speech`, `local`) and a voice
activity detector, set with `calls.stt`, `calls.tts` and `calls.vad`. They are gone: a
`config.json` that still has any of the three (whatever its value, `none` and `null`
included) does not start. `run` and `enroll` exit 1 and `status` shows, for the first of
`stt`, `tts`, `vad` it finds:

```text
invalid configuration: calls.stt is no longer supported: speech runs in a voice adapter. Configure calls.adapter (docs/agent-bridge.md §18, docs/voice-adapter-protocol.md).
```

Nothing changes silently. To migrate, remove the three fields and set `calls.adapter` to
a voice adapter that does the same job: one wrapping your whisper.cpp and piper, one
calling Azure AI Speech, or a realtime speech model. The adapter detects speech itself,
so `calls.vad` has no replacement in the bridge. Before:

```json
"calls": {
  "language": "en-US",
  "stt": { "provider": "local", "url": "http://127.0.0.1:8080/inference" },
  "tts": { "provider": "local", "command": ["C:\\Tools\\piper\\piper.exe", "--model", "C:\\Tools\\piper\\en_US-lessac-medium.onnx", "--output_raw"], "sample_rate": 22050 },
  "vad": { "silence_ms": 700 },
  "notify_by_call": { "after_minutes": 5 }
}
```

After:

```json
"calls": {
  "language": "en-US",
  "adapter": {
    "spawn": { "command": ["C:\\Tools\\voice\\voice-adapter.exe", "--config", "C:\\Tools\\voice\\adapter.json"] }
  },
  "notify_by_call": { "after_minutes": 5 }
}
```

A spawned adapter never gets `AZURE_SPEECH_KEY` (nor any variable named like a secret,
unless `env_allow` names it): give it its key in its own configuration, or under another
name listed in `calls.adapter.spawn.env_allow`. Without `calls.adapter` the bridge still
starts, but takes no calls (`Voice: off (no calls.adapter)`), and `calls.notify_by_call`
with `after_minutes` above 0 or `urgent` is an error.

### Voice adapter

The voice adapter does the speech, a separate process: it hears the call's audio and
says what was said, and it speaks the sentences the bridge sends it.
Any engine fits behind it (local models, a cloud service, a realtime speech model); the
wire is [`push-voice-adapter/1`](voice-adapter-protocol.md), which also shows how to
[write one](voice-adapter-protocol.md#14-writing-an-adapter). Everything else stays in the
bridge: answering and the policies, transcripts in the chat, prompts and steers, voice
answers to cards, spoken text and masking, barge-in, mute, activity and the tray. The
client is plain tokio and needs no feature, but only a build with `--features voice` takes
calls: without it no adapter is started and calls are not advertised.

**Required for calls.** `"calls": {"adapter": …}` alone is a calls section; without
`adapter` the bridge takes no calls and places none. `calls.notify_by_call` calls the
owner only while the adapter is ready with `stt` and `tts` and calls are advertised
(without `tts` it logs `call_held` `no_tts`).

**Spawn**: the bridge starts the adapter and speaks the protocol on its stdin (bridge →
adapter) and stdout (adapter → bridge).

```json
"calls": {
  "language": "en-US",
  "adapter": {
    "spawn": {
      "command": ["C:\\Tools\\voice\\voice-adapter.exe", "--model", "C:\\Tools\\voice\\models\\base.en"],
      "stderr": "log",
      "env_allow": ["HF_TOKEN"]
    }
  }
}
```

| Field | Meaning | Default |
|---|---|---|
| `command` | the program and its arguments, a list (argv), required, no empty item. Use an absolute path: a path with a folder that is relative is resolved against the bridge's working directory (warning `voice_adapter_relative_path` at start); a bare name is looked up on `PATH` | |
| `stderr` | `log`: appended to `logs\voice-adapter.log` in the bridge's folder (on Unix mode 0600); at each (re)start a log over 5 MiB is renamed to `voice-adapter.log.1`, replacing it (a failed rename only logs `voice_adapter_log_rotate_failed`). `discard`: nowhere | `log` |
| `env_allow` | variables to pass although their names look secret (below) | `[]` |

How it is started:

- **No shell.** The argv goes to the OS as is; `.bat` and `.cmd` files are refused (they
  run through cmd.exe): name an `.exe`, or an interpreter with the script as an argument.
  On Windows the adapter gets no window (`CREATE_NO_WINDOW`).
- **Environment.** The adapter inherits the bridge's environment without its secrets
  (names compared in any case): `CODA_SERVE_API_KEY` and `AZURE_SPEECH_KEY`, every name
  starting with `PUSH_` or `CODA_`, and every name containing `KEY`, `TOKEN`, `SECRET`,
  `PASS`, `CREDENTIAL`, `AUTH` or `CONNECTION_STRING`. A name in `env_allow` (any case on
  Windows, exact elsewhere) overrides only the last rule (the words); `env_allow` cannot
  name the fixed names or prefixes (a config error).
- **Killed with the bridge.** The child is killed when the bridge drops it; on Windows it
  also runs in a job object that kills whatever it started when the bridge goes away (a
  process it starts in the first moments, before it is assigned to the job, is not in it).
  Stopping: the bridge closes stdin, waits 2 s, then kills.

**Connect**: the adapter is already running (started by the user, a service, another
tool) and listens; the bridge connects and never creates the endpoint.

```json
"calls": { "adapter": { "connect": { "pipe": "push-voice-adapter" } } }
```

```json
"calls": { "adapter": { "connect": { "socket": "$XDG_RUNTIME_DIR/push-voice/adapter.sock" } } }
```

| Field | Meaning |
|---|---|
| `pipe` | Windows: the pipe's name only (1 to 200 of `A-Z a-z 0-9 . _ -`); the bridge opens `\\.\pipe\<name>` |
| `socket` | Unix: the socket's absolute path; a leading `$XDG_RUNTIME_DIR/` is replaced by that variable (unset: the connect fails and backs off). Adapters keep the socket in a private (0700) folder of their own inside the runtime dir, as the control channel does |

The adapter must run as the same user: on Windows the pipe server's user must be the
bridge's (or SYSTEM), on Unix the socket peer's uid must be the bridge's. Another user's
endpoint is refused ("… belongs to another user") and backed off from; the
[protocol](voice-adapter-protocol.md#2-transport) says how an adapter secures its side.

**Validation messages** (as for [the calls section](#the-calls-section)):

| Message | Fix |
|---|---|
| `calls.adapter.spawn.command is empty`, `calls.adapter.spawn.command[<i>] is empty` | the program, then its arguments |
| `calls.adapter.spawn.command[0] is a batch file; …` | an `.exe`, or the interpreter with the script as an argument |
| `calls.adapter.spawn.env_allow[<i>] must be an environment variable's name` | letters, digits and `_`, not starting with a digit |
| `calls.adapter.spawn.env_allow[<i>] names a variable the bridge never passes on` | the fixed names and the `PUSH_`/`CODA_` prefixes cannot be allowed |
| `calls.adapter.connect.pipe must be the pipe's name only: …` | `push-voice-adapter`, not `\\.\pipe\push-voice-adapter` |
| `calls.adapter.connect.pipe is only available on Windows`, `calls.adapter.connect.socket is only available on Unix` | |
| `calls.adapter.connect.socket must be an absolute path (it may start with $XDG_RUNTIME_DIR/)` | |

Unknown fields, both `spawn` and `connect`, or both `pipe` and `socket` are refused when
`config.json` is read.

**Lifecycle.** The adapter client is `starting` (spawning or connecting), then
`handshaking`: the bridge sends its hello and the adapter must answer with its own within
10 s (unknown message types before it are skipped; audio or any other known message is
fatal). Then it is `ready`: the bridge pings every 10 s; 30 s without a frame from the
adapter (or a write that cannot finish), more than 100 control messages in a second
(partial transcripts over 10 a second are dropped first), or a bad frame end the link. Any
end, and any failure to start, connect or shake hands, is one warning
`voice_adapter_failed` with a content-free `reason` and `retry_in_s` (the same reason
again within 60 s is logged at debug), and the state `backoff`: the bridge tries again
after 1 s, doubling up to 60 s; an adapter that stayed ready 60 s starts the delays from
1 s again. Each start after the first counts as a restart. When the bridge stops it ends
the running call with the adapter (`call_ended {reason: shutdown}`), stops the advertiser
(closing the client ends the advertisement), closes the link (queued messages are sent
first), and stops a spawned adapter.

**Advertising.** Calls are advertised (the owner's app sees the agent as callable) only
once the adapter has been ready **with `stt`** for 2 s, and withdrawn as soon as it is not
ready. The advertisement never changes while a call rings, is asked about, being answered
or placed, or runs, or while the bridge stops: it changes once the call ended. A change
makes the bridge reconnect to the server at once, identifying with the new capability
(`calls_advertised`, `on`); it is not a failure and waits for no backoff. An adapter
without `stt` is ready but calls are not advertised (`status` says so).

**During a call.** When the call connects, the adapter gets `call_started` with a fresh
random `call_id`, an empty `peer` (the bridge does not send the owner's name) and
`calls.language`; then the owner's audio (stream 0, a 1 s queue that drops the oldest
frames). Its `speech_started` is a barge-in (with `stop_speaking`) and the activity
"listening"; its final `transcript` is handled as described in
[the pipeline](#the-pipeline) (an agent.transcript card, a card's answer, a prompt or a
steer); partials only keep "listening", which ends 30 s after `speech_started` without a
final. Without `vad` the final transcript is the barge-in. The sentences to say become
`speak`s (the agent's replies interruptible, the bridge's own lines not; a text over 4,000
characters is split), at most 16 unfinished, the rest waiting in the bridge. The adapter's
audio (stream 1) plays in real time with at most 2 s buffered; after silence the first
frames wait up to 60 ms (a jitter buffer), so send a speak's first frames a little ahead.
Without `tts` nothing is spoken (the replies stay in the chat). The call's end sends
`call_ended` with `completed`, `failed` or `shutdown`.

**Fallback.** The call never fails because of the adapter; the chat carries on:

| Chat note | When | Then |
|---|---|---|
| "Voice adapter stopped; reconnecting…" | the adapter's link ended during the call, or none was ready when the call connected (a new call also waits up to 2 s for the previous call to give the adapter back) | the call stays up, chat-only: the agent's replies are posted but not spoken, nothing is heard (words said meanwhile are lost) |
| "Voice adapter is back." | a restarted adapter is ready: it gets `call_started` again with the same `call_id` | speech goes on |
| "Voice adapter cannot listen right now." | the adapter answered `engine_unavailable` | chat-only until the call ends or a restarted adapter takes it |

A `recognition_failed` makes the bridge say "Sorry, I couldn't make that out."; a
`synthesis_failed` skips that sentence.

**Status.** `push-agent-bridge status` asks the running bridge over the control channel
and prints the adapter's state on the `Voice:` line ([build with voice](#build-with-voice))
and its counters on a `Voice adapter:` line. The control channel's
[status object](#the-status-object) has `adapter` (state, name, version, capabilities,
backoff reason and `retryAt`, metrics) and the event `adapter_changed`; the tray's
Overview shows "<name> <version> (ready)", "starting" or "backing off (<reason>)".

**Metrics** (content-free): calls, frames to and from the adapter, dropped frames each way
(the 1 s queue to the adapter; beyond the 2 s playback buffer, wrong or late audio from
it), restarts, speaks, and two latencies as last/avg/max: speak latency (final transcript
→ its `speak`) and audio latency (`speak` → its first audio). The log has
`adapter_metrics` at each call's end (`call_id`, `frames_to_adapter`,
`frames_from_adapter`, `frames_dropped`, `speaks`, the latencies' last and max ms) and
`adapter_metrics_total` when the bridge stops (the totals, `restarts`, last/avg/max).
Other events: `adapter_ready` (`name`, `version`, `stt`, `tts`, `vad`),
`adapter_handshake_failed` and `adapter_link_closed` (`reason`), `voice_adapter_failed`,
`calls_advertised`, `adapter_call_started`, `adapter_lost`, `adapter_unavailable`,
`adapter_recognition_failed`, `adapter_synthesis_failed`, `adapter_listening_timeout`,
`adapter_overloaded`, `voice_adapter_job_failed`; at debug, `adapter_error` (`code`),
`adapter_unknown_type`, `voice_adapter_event_dropped`, `voice_adapter_killed`.

**Troubleshooting**

- *`voice adapter program <file> was not found`*: `command[0]` is wrong, or a bare name
  is not on the bridge's `PATH` (a Scheduled Task's `PATH` may differ from a console's):
  use an absolute path.
- *`no voice adapter is listening on \\.\pipe\<name>`* (or the socket's path): the adapter
  is not running, or listens on another name. The bridge keeps retrying (up to every
  60 s); start the adapter.
- *`\\.\pipe\<name> belongs to another user`*: a process of another user holds the name
  (or the adapter runs as another user): run the adapter as the bridge's user, or pick
  another name.
- *A version or format mismatch*: the reason names it with the protocol's code, e.g.
  `voice adapter speaks push-voice-adapter/2; this bridge speaks push-voice-adapter/1`,
  `voice adapter does not speak push-voice-adapter/1 (unsupported_protocol)` or
  `voice adapter cannot take the bridge's audio (16000 Hz, 1 channel(s), s16le, 20 ms
  frames) (unsupported_format)`. Update the adapter or the bridge; calls stay unadvertised
  meanwhile.
- *`the voice adapter sent a bad frame: …` right after the start* (spawn): the adapter
  prints something to stdout (a banner, a progress bar, a `print`), which is the protocol
  stream. Send everything else to stderr (it lands in `voice-adapter.log`).
- *`the voice adapter sent no hello within 10 s`*: the adapter must answer the hello
  within 10 s ([handshake](voice-adapter-protocol.md#6-handshake-and-versions)); one
  that loads a model first should answer at once and load afterwards (answering a call's
  speech it cannot handle yet with `engine_unavailable`).
- More cases, from the adapter's side: the protocol's
  [troubleshooting](voice-adapter-protocol.md#15-troubleshooting).
- Its own output: `logs\voice-adapter.log` (spawn with `stderr` `log`); more detail from
  the bridge with `PUSH_BRIDGE_LOG=debug`.

### The pipeline

The bridge carries the call's audio between the owner's app and the voice adapter and
keeps the conversation; the adapter does the speech. Call audio is 16 kHz mono in 20 ms
frames (the [media helper](calls.md#194-audio)), exactly the adapter's format, so neither
side converts it again. Up to 5 s of the owner's audio waits for the pipeline (newer
frames are dropped when it falls further behind), and a 1 s queue to the adapter drops
the oldest.

| In the bridge | In the voice adapter |
|---|---|
| answering and the [policies](#asking-before-answering), owner-only calls, busy and stranger declines | detecting speech (`speech_started`) |
| transcripts in the chat, prompts and steers, [voice answers](#voice-answers-to-cards) to cards | speech-to-text (final `transcript`s) |
| the replies turned into plain sentences, [masking](#9-security-model), mute and barge-in | text-to-speech (stream-1 audio for each `speak`) |
| activity labels, the tray's call controls, the [fallback](#voice-adapter) when the adapter goes away | its own engines, keys and logs |

**Listening.** The adapter says when the owner starts speaking (`speech_started`, with
`vad`) and what was said (a final `transcript`). A `recognition_failed` from it makes the
bridge say "Sorry, I couldn't make that out." Each final transcript is then:

1. posted to the owner's conversation as an
   [`agent.transcript@1`](rich-messages.md#agenttranscript1) card (`speaker: owner`, the
   text masked and cut at 4,000 characters; fallback "🎤 voice: <text>"; heading "🎤
   voice"), and kept in the [chat peek](#chat-peek) as the owner's text "🎤 <text>";
2. with a card open, an answer to the oldest one ([voice answers](#voice-answers-to-cards)),
   but only when the utterance began (its `speech_started`, or the final transcript's
   arrival without one) after that card was posted:
   words begun before the card existed were not meant for it ("yes, go ahead" said as a
   steer never approves a permission card that opened while it was transcribed);
3. otherwise sent to the agent as typed text is ([the owner's text](#6-behaviour), steps
   3 and 4): a prompt when the agent is idle, a steer while a turn runs (or waiting for it
   to end), held while Coda is paused. A transcript is **never a `/` command**, whatever it
   says.

**Speaking.** The agent's replies are still posted as usual; when the adapter has `tts`
they are also spoken, sentence by sentence as the text streams in, without waiting for the
turn to end. The Markdown becomes plain sentences:

| In the reply | Said |
|---|---|
| a fenced code block (```` ``` ```` or `~~~`) | "I've put the code in the chat." |
| a table (a line starting with `\|`, or a header row followed by `--- \| ---`) | "I've put a table in the chat." (the same note is not said twice in a row) |
| a heading, a list item (also a task list item) | a sentence of its own, ending with a period if it has none |
| a link, an image | its text |
| a bare URL, an autolink | "a link" |
| a code span | its content (a stray backtick is not said) |
| emphasis, strikethrough, quote markers, rules, HTML tags, backslash escapes | removed (`snake_case` and `2*3` stay) |

Sentences end after `.`, `!`, `?` or `…` and a space, but not after abbreviations such as
"e.g.", "i.e.", "etc.", "Dr."; a sentence over 300 characters is cut at a comma or space.
Background messages (`notify_user`) are spoken too: "<label>." then their text. What is
spoken is [masked](#9-security-model) like what is posted, before it reaches the voice
adapter: each complete line (and a background message's label) is masked before it is
read, so a secret is never split across sentences and code spans are masked too; a
masked secret is said as "hidden" ("api_key=hidden"). Each sentence becomes a `speak`
(the agent's replies interruptible, the bridge's own lines not), and the adapter's audio
is played in real time; a sentence the adapter fails on (`synthesis_failed`) is skipped.

**Barge-in.** When the owner starts speaking (the adapter's `speech_started`; without
`vad`, its final transcript) while the agent's
voice is queued or playing, the agent stops at once: the sentences waiting and the audio
already queued are dropped (the adapter gets `stop_speaking`), and the rest of that reply
is not spoken. The turn itself goes
on. When the owner's utterance ends it is transcribed and steered into the running turn
(or prompts, if the turn ended); from then on the reply is spoken again, from its next
sentence (the rest of the line that was arriving is skipped too). A code block or table
that was open stays one: its remaining lines stay silent and what follows its end is
spoken. Speech while the agent is silent is simply an utterance and changes nothing in how
the reply is spoken (a code block streaming silently stays silent).

**Activity** during a call, when no card is open and Coda is not paused (cards and a pause
keep their labels, [activity](#6-behaviour)):

| State | Label | When |
|---|---|---|
| `working` | `speaking` | the agent's voice plays |
| `working` | `listening` | the owner speaks |
| `working` | `thinking` | a turn runs |
| `working` | `on a call` | otherwise |

### Voice answers to cards

When a card opens during a call (and it is the only open one), the reply so far is spoken,
then its question; at the greeting and after each voice answer, the oldest open card's
question is spoken. A spoken transcript answers the **oldest** open card, if the utterance
began after that card was posted; an utterance begun earlier goes to the agent (a prompt
or steer) and the card stays open, its question spoken as usual.

| Card | Said |
|---|---|
| permission (`agent.approval@1`) | "Coda wants to use <tool>. Approve or reject?" |
| `/mode bypass`, `/yolo` | "Turn off approvals until you start a new session? Approve or reject?" |
| plan (`agent.plan-approval@1`) | "The plan is in the chat. Approve or reject?" |
| question, 1 to 10 options | "<question> Option one: <label>. Option two: <label>." then "Or say your own answer." when free text is allowed and "You can pick several." for a multi-select (the question at most 500 characters, masked) |
| question, no options | "<question>" |
| question, more than 10 options | "<question> The options are in the chat." |

What is heard: words are compared whole, in lower case, without punctuation; leading "um",
"uh", "er", "erm", "hmm", "hm" are ignored.

- **Approvals** (permission, bypass, plan) need exactly one of approve ("approve",
  "approved", "yes", "yeah", "yep", "yup", "sure", "ok", "okay", "allow", "accept",
  "accepted", "confirm", "go ahead", "do it", "sounds good") or reject ("reject",
  "rejected", "no", "nope", "deny", "denied", "decline", "declined", "cancel", "stop",
  "don't", "do not", "not now", "never"). A negation ("not", "never", "cannot", "nor",
  "n't") never approves: "I can't approve that" is asked again. A question (starting with
  "is", "are", "can", "could", "should", "would", "will", "do", "does", "what", "why",
  "how", "which", "who", other than "do it" or "do not") is asked again. Unclear: "Sorry,
  I didn't catch that. Say approve or reject."
- **Questions** take options by number ("option two", "number 2", "choice three", "the
  second one", "second option", "2nd", a bare "two") or by their label as whole words. A
  multi-select takes all it hears; a single choice that hears several asks "Sorry, which
  one? Say the option's number." Nothing heard: the transcript is the answer when the card
  allows free text (or has no options), else "Sorry, which option? Say its number, like
  option one." A transcript with no words at all on a free-text question: "Sorry, I didn't
  catch that. Say your answer again."

Asked again, the card stays open. Answered, the bridge settles the request as a typed
answer does (the agent gets the same answer the card's buttons would give), edits the card
into a closed `agent.reply@1` titled with the card's title and saying "🎤 Answered by
voice: <label>." (Approve, Reject, the chosen labels joined with ", ", or the free text;
masked, at most 200 characters), and says "Approved.", "Rejected." or "Got it: <label>.",
then the next open card's question. A question shown as text only (no options or more than
10) has no card to edit.

The card is **edited, not answered**: the bridge sent it, and the server refuses the
sender's own answer (and tpl/1's ANSWER format has no place to say "by voice"). An
unanswered card may be edited by its sender; the new body has no buttons, so the server
refuses answers from the app to it afterwards. The owner can still answer on screen or
type an answer during a call, as without one.

### Asking before answering

With `"answer": "ask"` the owner's call is not answered at once: the bridge posts a
question card in the owner's conversation, so it shows on every device of the owner
(also the one calling):

> 📞 Owner is calling Coda in app — answer? **[Answer]** **[Decline]**

- **Answer** before the ring ends (45 s by default, the server's `calls.ringSeconds`):
  the bridge answers and greets as usual; the card keeps the owner's choice, as any
  answered card.
- **Decline**: the call is declined (the caller hears `declined`).
- No answer: the server ends the call as missed and the card is edited into a closed
  reply "📞 Missed call."; if the server's end never arrives, the bridge declines 5 s
  after the ring's end (at most 2 minutes after the call came in). The owner hanging up
  first: "📞 The call was cancelled.". The tray's **Hang up** declines it: "📞 Declined.".
- An answer after that is refused by the card (it is closed). While the bridge waits for
  the answer, another call is declined busy and the bridge places no call itself.

The card is not a request of the agent's: it does not change the activity, the
control channel's `openRequests`, typed answers or voice answers. Its state is in the
[chat peek](#chat-peek) like any card, and the control channel shows the call as
`ringing`, `incoming`.

### The agent calls you

`calls.notify_by_call` lets the bridge call its owner (with push-sdk's media helper as
the caller) instead of only waiting for a card to be answered:

```json
"calls": {
  "adapter": { "spawn": { "command": ["C:\\Tools\\voice\\voice-adapter.exe"] } },
  "notify_by_call": {
    "after_minutes": 5,
    "urgent": true,
    "min_interval_minutes": 10,
    "do_not_disturb": ["22:00-07:00", "sat,sun 00:00-24:00"]
  }
}
```

| Field | Meaning | Default |
|---|---|---|
| `after_minutes` | a permission request, question or plan (not `/mode bypass`, which the owner asked for) still unanswered after this many minutes makes the bridge call; `0` never calls for them. At most 8: a card expires after 10 minutes, and the call must ring and talk before then | `0` |
| `urgent` | a background message (`notify_user`) Coda marks urgent makes the bridge call | `false` |
| `min_interval_minutes` | at most one call per this many minutes, whatever it is about (counted from when each call was placed, answered or not); 10 to 1440 | `10` |
| `do_not_disturb` | local time ranges without calls, a string or a list: `"HH:MM-HH:MM"` every day, or with days first, `"mon-fri 22:00-07:00"`, `"sat,sun 00:00-24:00"`, `"mon,wed-fri 12:00-13:00"` (`mon` … `sun`, ranges may wrap, `fri-mon`). A range whose end is not after its start runs past midnight into the next day; its days are the days it starts on (`"fri 22:00-07:00"` covers Friday 22:00 to Saturday 07:00). Times are the computer's local time zone, with its daylight saving | none |

`notify_by_call` needs `calls.adapter` (validation says so), and at run time an adapter
that can listen and speak: a call nobody hears or that cannot say why it rings is
pointless. The bridge calls only while the [voice adapter](#voice-adapter) is ready with
`stt` **and** `tts` and calls are advertised; a ready adapter without `tts` holds the call
back (`call_held` `no_tts`, logged once). With both `after_minutes` 0 and `urgent` false
the section does nothing.

**When it calls.** Every half second the bridge checks the open requests. When one has
waited `after_minutes`, no call runs, rings or is being answered, the messenger is
connected and the agent is not paused, it calls, unless the last call was placed less
than `min_interval_minutes` ago or it is do-not-disturb time (then it calls once that is
over, if the request is still open; the log says `call_held` with `reason` `too_soon` or
`do_not_disturb` once per change). Each request starts at most one call. An urgent
message calls at once under the same rules (or not at all: the message is in the chat).

**Urgent.** Coda's `notify_user` has no urgency field yet; the bridge treats a message as
urgent when its event carries `"urgent": true` or `"priority": "urgent"` / `"high"` (for
when Coda adds one), or when its label starts with "urgent" (`"urgent"`, `"URGENT: build
broken"`, `"[urgent] prod is down"`): the agent can be told to label urgent
notifications that way.

**The call.** The owner's devices ring like for any call (the activity says `calling
you` unless a card is open). When the owner answers, the bridge says who calls and why,
then the question:

- about a request: "Hi, it's Coda in app. A request has waited 5 minutes for your
  answer." and the oldest open card's question ("Coda wants to use Bash. Approve or
  reject?"), answered by voice as in any call ([voice answers](#voice-answers-to-cards));
  if it was answered meanwhile: "It was answered meanwhile; nothing is waiting.";
- about urgent news: "Hi, it's Coda in app. I have something urgent.", the message's label
  and text (masked as always), then the oldest open card's question, if any.

From then on it is an ordinary call: the owner can talk to the agent, answer cards, hang
up.

**Not answered.** A call that ends before the owner picks up (missed after the ring,
declined, busy, unreachable, failed) changes nothing about what it was for: the card
stays open and takes answers as before, the message stays in the chat. The bridge posts
one note, "📞 I called about the open request (no answer). It is still open above." (or
"(you declined)", "(you were busy)", "(you could not be reached)", "(the call failed)";
for urgent news, "📞 I called about "<label>" (…)."). A call still ringing is cancelled
(without a note) when nothing is waiting any more, for example when the owner answered
the card on screen, and when the tray's **Hang up** cancels it. If the phone cannot place
the call at all (offline, calls not available on the server), the log says
`call_dial_failed` with the code and the card stays as it is.

### Call controls in the tray

The control channel reports the call ([§14](#14-control-channel-for-other-tools): `call`
in the status, `call_changed`), so the tray shows each agent's call (ringing, connecting,
in a call, speaking or listening, muted) and offers two actions ([§13](#13-the-tray-app-push-agent-tray)):

- **Mute agent** / **Unmute agent** (`mute`): the agent stops speaking at once and says
  nothing more in this call (replies, its own sentences, card questions) until unmuted;
  the owner is still heard, transcripts and voice answers work, and everything is still
  posted in the chat. For connecting and running calls.
- **Hang up** (`hangup`): hangs up a running call, cancels the bridge's call while it
  rings, declines a call waiting for the ask card (the card says "📞 Declined."), or drops
  one being answered.

Both answer `no_call` when there is no call.

### Privacy: what the adapter receives

The call's audio travels end to end encrypted (DTLS-SRTP) between the owner's app and the
bridge; the server only relays the signalling ([calls privacy](calls.md#9-privacy)). From
there the bridge gives the voice adapter only what speech needs:

| The adapter receives | Never |
|---|---|
| the call's audio (16 kHz mono, as it arrives, from the call's connection to its end) | message history, files, the agent's tools or anything else of Coda's |
| the text of each sentence to speak (the agent's reply, the bridge's own sentences such as card questions), masked | credentials, tokens, keys, or ids that link to the messaging server |
| `calls.language`, a random `call_id` per call, and the caller's display name field, which the bridge sends empty | the owner's name, handle or file paths |

- The adapter runs as the same user: started by the bridge without a shell and with an
  environment stripped of the bridge's secrets ([spawn](#voice-adapter)), or reached on a
  pipe or socket private to the user ([connect](#voice-adapter)). Where it sends the audio
  and the text (a local engine, a cloud speech service) is up to the adapter: check its
  own documentation.
- The bridge holds no speech credentials; the adapter keeps its own.
- The bridge persists nothing of a call: no audio, no transcripts beyond the
  `agent.transcript@1` messages it posts (masked, and stored on the server like every
  message: [security](#9-security-model)).
- Logs are content-free: names, codes, counts and durations, never audio, transcripts or
  spoken text. Events: `voice_enabled` (`adapter`: `spawn` or `connect`, `answer`,
  `notify_by_call`), `voice_off` (`reason` `no_adapter`), `voice_unavailable`,
  `call_answering`, `call_answered` (`buffered`), `call_connected`, `call_refused`
  (`reason`: `stranger`, `voice_off`, `busy`), `call_answer_failed` (`code`),
  `voice_transcript` (`chars`), `voice_barge_in`, `call_chat_only` (`reason`:
  `adapter_lost`, `adapter_unavailable`), `card_answered_by_voice` (`values`),
  `voice_call_ended` and `call_ended` (`reason`, `duration_s`), `call_hung_up` (the bridge
  stopped); the [voice adapter](#voice-adapter)'s events; for the
  [policies](#the-agent-calls-you): `call_asking` (`card`), `call_ask_answered`
  (`answer`), `call_ask_ended` (`reason`), `call_ask_expired`, `call_ask_failed`,
  `call_dialing` (`reason`: `request` or `urgent`), `call_ringing` (`buffered`),
  `call_held` (`reason`), `call_dial_failed` and `call_dial_cancelled`,
  `call_unanswered` (`reason`), `call_withdrawn`, `urgent_call_skipped`, `call_muted`
  (`muted`), `call_hangup_requested`; at `debug`, `call_ignored`,
  `call_event_ignored`, `call_decline_failed` and `call_answer_dropped`.

### Testing

- **The test adapter.** Speech is tones: `crates/push-voice-adapter-testkit` is a
  scripted voice adapter that hears a word per frequency (by Goertzel) and speaks a 600 Hz
  tone, both surviving real Opus media. Its library (`ScriptedAdapter`) runs in process
  over in-memory pipes; its binary is spawned or connected to like a real adapter, and
  plays faults on demand (a crash, a refused format, another protocol version, a stall, a
  flood, no hello). What it heard and did is in its `Log` (content-free; the texts it
  spoke stay in memory, `Log::spoken()`, for in-process assertions only).
- **Fixtures** (`crates/push-voice-adapter-testkit/fixtures/*.json`, its `fixture`
  module, see the folder's README): the owner's side as `words` (`{hz, text}`) and a
  `script` of `say` (`hz`, `ms`) and `silence` (`ms`) steps at amplitude 8000.
  Frequencies lie between 350 and 2400 Hz, at least 200 Hz apart across fixtures, and
  never within 150 Hz of 600 Hz (the agent's voice must not be heard as the owner's
  words); each `say` is followed by at least 1 s of silence. Fixtures: `prompt`,
  `barge_in`, `approve`, `option_two`, `ambiguous`.
- **Unit and core tests** (`cargo test -p push-bridge`, no feature needed): spoken text,
  voice answers, the adapter client (spawn, connect, the link, the supervisor and the call
  router, against the testkit); the core with a `FakePhone`, the testkit's adapter,
  `TestBackend` and `TestMessenger` in paused time (`src/core/voice_tests.rs`): greeting,
  declines, spoken replies, barge-in, voice answers, activity, masking and the peek; the
  policies: ask mode (answer, decline, a
  busy second call, the ring ending, the bridge's own deadline), a request unanswered
  calling the owner who approves by voice, missed, declined and unreachable calls leaving
  the card open (and never calling twice for it), the rate limit, do-not-disturb hours,
  urgent and ordinary news, a ringing call withdrawn when the card is answered on screen,
  and the tray's `mute` and `hangup` (`src/voice/policy.rs` has the do-not-disturb ranges,
  across midnight, days and time zones). `tests/bridge.rs` runs a call through the
  public API. CI's `calls-media-windows` job also runs the `voice::` tests with the feature
  on Windows.
- **End to end** (`crates/push-smoke/tests/bridge_voice.rs`, CI job `bridge-voice`): the
  whole stack in one process, the owner calling with push-sdk's `MediaCall`, the bridge
  enrolled with a software key and running with `SdkMessenger`, `MediaPhone`, a scripted
  backend and the testkit's adapter (spawned), over real media: greeting, a prompt, a
  spoken reply, barge-in, a voice approval, a stranger refused, the hang-up. A second test
  runs the policies with a
  10 s ring: `ask` (the call waits for the card's "Answer"), a card unanswered for 3 s
  making the bridge call the owner, who answers, hears the summary and approves by voice,
  and a call the owner lets ring out, after which the card is still open and takes the
  answer. `crates/push-smoke/tests/bridge_voice_adapter.rs` (same job) tests the adapter
  link itself with the testkit's binary (the tests build it): spawned and connected, a
  crash mid-call (the owner is told, the restarted adapter takes the call
  again), no adapter and a refused audio format (calls not advertised, one warning), a
  final transcript becoming a `speak` within 100 ms, the agent's voice without gaps over
  40 ms, and a log free of what was said. Locally, with Docker:

  ```powershell
  $env:PUSH_BRIDGE_VOICE = "1"
  cargo test -p push-smoke --test bridge_voice --test bridge_voice_adapter -- --nocapture --test-threads 1
  ```

- **Real speech engines** are tested in their own adapters (for example, a private
  implementation), against the [protocol](voice-adapter-protocol.md); the bridge's tests
  need none.

### Limitations

- The ask card goes to the owner's conversation, so it shows on every device of the
  owner, the calling one too; push-messenger cannot address one device.
- Coda's `notify_user` has no urgency field yet: urgent means a label starting with
  "urgent" (or the forward-compatible `urgent` / `priority` fields).
- A card still expires after 10 minutes, during a call too; hence `after_minutes` of at
  most 8. A request's call happens once; a missed call is not tried again for it.
- Do-not-disturb uses the computer's local time zone only (no `time_zone` field).
- One call at a time; no video; the owner's push notifications do not ring (calls ring
  only on connected devices, as for every call).

## 19. Echo backend

`"backend": "echo"` runs a deterministic test agent instead of Coda: no engine, no model,
no credentials beyond the bridge's own agent key. It exists for contract checks and
manual tests from a dev box (texts, cards and their answer rules, activity, the tray,
voice calls) where a cloud-hosted test agent cannot sign in. Everything else works as for
Coda: `api_url`, the key and `keygen`, `enroll --code`, the state folder, the control
channel, the tray, `calls`. `roots` and `default_folder` are still required (the core
needs a folder); any existing folder will do, nothing is read or written there.

```json
{
  "api_url": "https://push.example.com",
  "key": { "name": "push-messenger-echo" },
  "roots": ["C:\\src"],
  "default_folder": "C:\\src",
  "backend": "echo",
  "echo": { "answerTimeoutS": 300 }
}
```

`echo` is optional; `answerTimeoutS` (default 300) must be 10 to 3600 seconds
(`enroll` and `run` refuse anything else; `status` says what is wrong). Use `--dir` for a folder of its own, and a
`key.name` of its own, as for [several agents](#several-agents-on-one-machine).

**Behaviour.** Texts are matched after trimming, in any case.

| The owner sends | The echo agent |
|---|---|
| `ask` | posts `agent.question@1` "Red or blue?" (Red, Blue); activity `waiting` / `question open`. The answer (a button, a label or number, or any other text, which is the free-text answer) gets "you chose: <answer>" |
| `pick` | posts `agent.choose@1` "Pick any colours." (Red, Blue, Green; any number of them; typed answers separated by commas); "you chose: Red, Green" in option order |
| anything else | replies `agent.reply@2` (Markdown) "echo: <text>", with activity `working` / `echoing`; a text over 4,000 characters continues in further replies, as every turn does |

- **Who may talk to it:** the bridge's rule, unchanged: only the agent's owner, in the
  owner's direct conversation. Anyone else, an accepted contact too, gets "This agent is
  private." and nothing reaches the backend. So the one who asked is always the owner;
  anyone else's answer to the card is refused (`answer_refused`) and the card stays open.
  While a card is open, the owner's next text answers it.
- **Timeout:** an unanswered card closes after `answerTimeoutS`: "No answer; skipped".
- **`/stop`** while a card is open declines it ("Declined: the turn was stopped.").
- **Old texts:** a text stored more than 60 s before the bridge started is skipped
  (logged as `old_text_skipped`, without content), so a restart does not replay the
  backlog.
- **Commands:** the echo agent reports no capabilities. `/help`, `/status` and `/stop`
  are the core's and work; every other command (`/new`, `/cd`, `/sessions`, `/model`,
  `/tasks`, `/mode`, …) answers "Not supported by Echo.". Any other `/text` is echoed.
- **Calls:** with `calls.adapter` configured (a build with `--features voice`), the owner's
  call is answered as for Coda and each final transcript is echoed and spoken back
  through the adapter as "echo: <text>". Without an adapter, calls are not advertised.
- **Tray:** the tray shows "Echo" where it would show "Coda" ("Restart Echo", "Echo
  restarting", "Echo version").
- **Not modelled:** sessions, folders, tasks, background messages, plans and approvals;
  `pause` stops it until `resume` (it has no program to watch and no state to save).

`cargo test -p push-bridge --lib echo` runs its tests (the core with a `TestMessenger`,
and a call through the testkit adapter).
