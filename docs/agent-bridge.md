# Agent bridge: a local coding agent as a private contact

`push-agent-bridge` (crate `push-bridge`) lets the owner of an AI coding agent that runs on
their own Windows machine reach it from a phone or a browser through push-messenger. The
agent becomes a private [owned agent](entra-sign-in.md#owned-agents-and-app-only-authentication-opt-in)
contact: the owner sends it tasks, watches it think and work, approves its edits and
commands, answers its questions and approves its plans from cards, and controls the session
with `/` commands. Coda is the first backend. Design: the implementation plan
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

## 1. Overview

| | |
|---|---|
| What runs | one `push-agent-bridge.exe run` per agent, as the logged-on user, started at logon by a Scheduled Task |
| Identity | an owned agent signed in with [agent-key](agent-key-sign-in.md): a non-exportable P-256 key in the TPM (or the software key storage provider) |
| Who may use it | only the agent's owner (`owner_user_id` of the agent's own profile); anyone else gets "This agent is private." |
| The agent | `coda serve` over stdio (JSON-RPC 2.0 with Content-Length framing), started and restarted by the bridge |
| Replies | `agent.reply@1` cards: thinking, tool steps and text, edited as the turn runs |
| Questions to the owner | `agent.approval@1`, `agent.question@1`, `agent.choose@1`, `agent.plan-approval@1` ([built-ins](rich-messages.md#6-built-in-templates)) |
| Status | live activity: working, waiting, idle ([activity](activity-presence.md#7-agents)) |
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

**Adding a backend** (Cortex, for example) needs no second bridge:

1. Implement `Backend` in a new module: map the agent's protocol to the calls and events
   above, report only the capabilities it has, and answer each `Responder`.
2. Add a `BackendKind` variant and its config section next to `coda` in `config.rs`
   (`"backend": "<name>"`, `"<name>": {…}`), with `Config::validate` checking the section.
3. Build the backend for that kind in `main.rs`, where `CodaBackend::new` is built today.
4. Test it against the core with the core's tests as a model.

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
  SQLite).

## 4. Setup on Windows

The steps use the default folder `%LOCALAPPDATA%\push-messenger\coda-bridge`. Add `--dir
<folder>` to every command to use another one.

### 1. Build the bridge

```powershell
cargo build --release -p push-bridge
Copy-Item target\release\push-agent-bridge.exe C:\Tools\push-agent-bridge.exe
```

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
  "coda": { "exe": "C:\\Tools\\coda\\coda.exe", "args": [] }
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
| `backend` | yes | `coda`, the only backend | |
| `coda.exe` | with `coda` | the path to `coda.exe` (a bare name is looked up on `PATH`) | |
| `coda.args` | no | extra arguments after `serve`, as a list | `[]` |

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
| `status` | prints the configuration path (and what is wrong with it), the key and its thumbprint, the agent user, the device, the folder, the sessions per folder and the log's path; no secrets |
| `help`, `--help`, `-h` | prints the usage |

`--dir` may come anywhere; `--dir=<folder>` and `--code=<code>` work too. `keygen`,
`thumbprint` and `status` read `config.json` without validating it; `enroll` and `run`
validate it first. `enroll` and `run` write the log; the others print only.

| Exit code | Meaning |
|---|---|
| 0 | done (`run`: stopped by Ctrl-C) |
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
the owner's prompt started it or the engine did (after a background task finished). Each
turn is **one `agent.reply@1`** (untitled), sent at once and then edited at most every 2 s
while something changed, plus once at the end:

| Part | Content |
|---|---|
| `thinking` | the latest 4,000 characters of the turn's thinking (first reply of the turn only) |
| `steps` | at most 50 tool calls; when full, the oldest finished step makes room. Each is labelled `<tool> <target>` (the path, command, query, pattern, URL or description from the call's input), masked, one line, at most 80 characters; a subagent's tool is `<task>: <tool> <target>`. Status `running`, then `done` or `failed`, with the duration as the detail |
| `text` | the assistant's text. Past 4,000 characters the reply is final and the text continues in a new `agent.reply@1` (running steps move on with it) |

An interrupted turn ends its text with "(stopped)" and marks running steps failed. If Coda
exits mid-turn, the text ends with "(Coda stopped; restarting)". Coda's `event/error`
becomes a line in the reply (or a reply of its own between turns). A body over the 32 KiB
RICH limit drops the thinking first, then shortens the fallback, then drops the oldest
steps.

**Background work.** Each `notify_user` message (`event/agentMessage`) becomes its own
`agent.reply@1` titled with its label. Its cursor and Coda's engine instance are stored;
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
| `waiting` | `approval needed` (permission, `/mode bypass`), `question`, `plan approval` | a card is open (the oldest one) |
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
retried with the same backoff.

**Strangers.** Text and cards from anyone but the owner, in any conversation, get the plain
text "This agent is private." at most once per sender per hour; nothing they send reaches
Coda, and their answers to the owner's cards are ignored.

## 7. Cards

Every request from Coda becomes a card in the owner's conversation, posted after the reply
so far. A card waits **10 minutes**.

| Coda request | Card | Answer back to Coda |
|---|---|---|
| `request/permission` | `agent.approval@1`: title "Allow <tool>?", details "Coda wants to use <tool> in <folder>.", preview = the masked `inputPreview` (at most 4,000 characters) | Approve → `{allow: true}`. Reject, typed text other than `yes`/`y`, or no answer → `{allow: false}` |
| `request/question`, single choice, 1 to 10 options | `agent.question@1` | the chosen label → `{answer}` |
| `request/question`, multi-select, 1 to 10 options | `agent.choose@1` | the chosen labels in option order, joined with ", " |
| `request/question` with no options or more than 10 | no card: a reply titled "Question" with the numbered options | typed text only |
| `request/planApproval` | `agent.plan-approval@1`, title "Approve the plan?", the masked plan. A plan over 4,000 characters is cut with "(The plan is cut here; the full text follows.)" and follows in full as replies titled "Plan (1/N)" | Approve → `{approve: true}`; Reject, any typed text, or no answer → `{approve: false}` |

**Typed answers** (the oldest open request): a permission or `/mode bypass` takes `yes` or
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

Only the owner's commands run. Arguments are checked; a bad one is answered with the usage
line (`Usage: /effort <low|medium|high|xhigh|max|auto>`). Command names are case-sensitive:
`/NEW` goes to Coda as a prompt. Results come back as masked `agent.reply@1` text.

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
| `/mode bypass` | posts the approval card "Turn off approvals?" first; Approve → `bypassPermissions` ("Approvals are off until /new."). Reject or a timeout leave approvals on. `/new` resets it (and a bridge restart or `/cd` does too); `/status` shows it |
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
| Approval bypass from a stolen messenger session | `/mode bypass` needs a confirmation card; `default_permission_mode` cannot be `bypassPermissions`; `/new`, `/cd` and a bridge restart reset it; `/status` shows it |
| Folder escape | `/cd` and `default_folder` accept only real paths (after symlinks, junctions and `..`) inside a root, compared by path component (`C:\src2` is not inside `C:\src`) |
| Command injection | Coda gets text over JSON-RPC, never through a shell; `coda serve` is started with its arguments as a list |
| Credentials in Coda's hands | The bridge never sets `CODA_SERVE_API_KEY` (an LLM credential in Coda's engine) and removes it from Coda's environment; stdio needs no key |
| Logs | Names, codes, counts and durations only: never message bodies, previews, plans, typed answers, tokens or assertions |
| Secrets at rest | Tokens stay in memory; the SDK's database is in memory; `state.json` holds ids only |

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
| `agent-key.p8` | only with `"storage": "software-file"`: the private key as a plain PKCS#8 file |

With `"storage": "windows"` the key is in the user's CNG key store under `key.name`, not in
the folder. Log events include `started`, `signed_in`, `bridge_started`, `turn_started`,
`turn_ended`, `request_opened`, `card_answered`, `answer_refused`, `card_expired`,
`stranger_refused`, `command` (its name only), `backend_exited`,
`backend_restart_scheduled` (`delay_ms`), `backend_start_failed`, `session_not_found`,
`sign_in_failed`, `signed_out`, `send_failed` and `exit` (`code`).

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
| "spawn failed (spawn): cannot start Coda: …; retrying." | Coda is not at the path in `coda.exe` (or a bare name is not on the user's `PATH`). Fix `config.json`; the bridge keeps retrying with backoff meanwhile |
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
