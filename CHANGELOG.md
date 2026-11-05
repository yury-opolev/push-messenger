# Changelog

Notable changes for consumers of push-messenger. Newest first.

## Unreleased

### Added

- **Echo backend: accepted contacts** — the echo test agent now also answers the
  agent's accepted contacts (server-side relationship; requests, blocked users and
  strangers still get "This agent is private."), each in their own direct conversation.
  Every `ask`/`pick` card belongs to whoever asked: anyone else's answer is ignored and
  the card stays open, the asker's text answers it, and each card times out on its own.
  Contacts may run only `/help` and `/status`; other commands and calls stay the owner's.
  New defaulted `Backend` hooks `accepts_contacts()` / `contact_prompt()` and events
  `ContactReply` / `ContactRequest` (Coda does not opt in and stays owner-only);
  `Messenger::contact_state`. The echo backend may leave out `roots` and
  `default_folder` (it then works in the bridge's folder).

- **Echo backend for push-agent-bridge** (`"backend": "echo", "echo": {"answerTimeoutS":
  300}`): a deterministic, credential-free test agent that signs in with the bridge's
  agent key and runs under push-agent-tray. It echoes the owner's texts as `agent.reply@2`
  "echo: <text>" (activity "echoing", long echoes paged), posts `agent.question@1` on
  `ask` and the built-in multi-select `agent.choose@1` on `pick` ("you chose: …";
  activity "question open"; the owner's free text answers), skips an unanswered card
  after `answerTimeoutS` (10–3600 s) with "No answer; skipped", ignores texts stored more
  than 60 s before it started, reports no capabilities (only `/help`, `/status`, `/stop`
  work), and with `calls.adapter` speaks each final transcript back as "echo: <text>".
  The owner and the agent's accepted contacts reach it ([echo backend](docs/agent-bridge.md#19-echo-backend)).
- **Backend trait hooks** (all defaulted, Coda unchanged): `answer_timeout()`,
  `activity_labels()` and `skip_texts_older_than()`.
- **Agent bridge voice adapter client** (second of three steps that move speech out of
  push-agent-bridge): the new `calls.adapter` runs a call's speech in a separate voice
  adapter process speaking [`push-voice-adapter/1`](docs/voice-adapter-protocol.md).
  `{"spawn": {"command": [...], "stderr": "log"|"discard", "env_allow": [...]}}` starts
  it (argv only, never a shell, no window, batch files refused, the environment stripped
  of the bridge's secrets: `CODA_SERVE_API_KEY`, `AZURE_SPEECH_KEY`, `PUSH_*`, `CODA_*`
  and names containing `KEY`, `TOKEN`, `SECRET`, `PASS`, `CREDENTIAL`, `AUTH` or
  `CONNECTION_STRING`; stderr to `logs/voice-adapter.log`, rotated at 5 MiB at each
  start; killed with the bridge, on Windows through a job object) and speaks on its
  stdin/stdout; `{"connect": {"pipe": "<name>"}}` (Windows) or `{"connect": {"socket":
  "<path>"}}` (Unix, `$XDG_RUNTIME_DIR/` allowed) connects to a running adapter of the
  same user (Windows: or SYSTEM). The bridge supervises it (handshake within 10 s,
  ping/pong with a 30 s dead-peer timeout, rate caps, restarts with backoff from 1 s to
  60 s), advertises calls only while the adapter has been ready with `stt` for 2 s and
  never changes that during a call, and keeps a call up when the adapter goes away:
  chat-only with "Voice adapter stopped; reconnecting…", then "Voice adapter is back."
  once a restarted adapter took the call (or "Voice adapter cannot listen right now." on
  `engine_unavailable`). `push-agent-bridge status` shows the adapter's state and
  counters (from the running bridge); the control channel's `status` gains a
  content-free `adapter` (state, name, version, capabilities, backoff reason and
  `retryAt`, metrics) and the event `adapter_changed`; the tray's Overview shows it.
  Metrics: frames each way, dropped frames, restarts, final transcript → `speak` and
  `speak` → audio latencies, logged per call (`adapter_metrics`) and at shutdown
  (`adapter_metrics_total`). The client needs no cargo feature (MSRV 1.85); taking calls
  still needs `--features voice`. A new end-to-end test
  (`crates/push-smoke/tests/bridge_voice_adapter.rs`, job `bridge-voice`) covers spawn
  and connect modes, a crash mid-call, a missing adapter, a refused format, timing and
  content-free logs. Guide: [agent bridge §18](docs/agent-bridge.md#voice-adapter).
- **push-sdk `Client::set_calls_advertised`** (and `Client::calls_advertised`): start or
  stop advertising calls at runtime. A change while connected reconnects at once and
  identifies with the new capability, without backoff and without counting as a failure;
  `Calls::available()` follows what the current connection advertised. Never change it
  during a call. See [docs/calls.md §18](docs/calls.md#18-push-sdk).
- **`push-voice-adapter-testkit`** (`crates/push-voice-adapter-testkit`): a scripted
  voice adapter for tests, as a library (`ScriptedAdapter`, in-process over in-memory
  pipes) and a binary (spawn mode on stdin/stdout, or `--pipe`/`--socket`): tones in,
  transcripts out; text in, a 600 Hz tone out; crashes, refused formats, other protocol
  versions, stalls, floods and a missing hello on demand; `--events` and `--dump-env`
  for assertions. See [docs/voice-adapter-protocol.md](docs/voice-adapter-protocol.md).

- **Voice adapter protocol** (`push-voice-adapter/1`, first of three steps that move
  speech out of push-agent-bridge into a separate, swappable voice adapter process): the
  new crate `crates/push-voice-adapter-proto` (MSRV 1.85, no media dependency) with the
  frame codec (`u32` big-endian payload length, a kind byte, JSON control or audio
  payloads of at most 64 KiB; audio frames carry `stream`, `seq` and `timestamp_ms` and
  16 kHz mono s16le samples in 20 ms frames), an incremental sans-IO decoder that refuses
  oversize frames from the header alone, blocking and tokio (cancel-safe) readers and
  writers, typed control messages for both directions (`hello`, `call_started`,
  `speech_started`, `transcript`, `speak` of at most 4,000 characters, `speak_done`,
  `stop_speaking`, `call_ended`, `error`, `ping`/`pong`) that ignore unknown fields and
  message types, validation of every cap with content-free errors and `Debug` output that
  never shows transcripts or speech text, protocol version negotiation, and the protocol's
  limits and timers. JSON Schemas of every control message are generated from the types
  and checked in under `docs/schemas/voice-adapter/v1/` (a test keeps them current).
  Tests: unit and property tests (split and merged frames, length limits, bad kinds,
  arbitrary bytes, round trips) and two cargo-fuzz targets, run briefly by the new CI job
  `voice-adapter-fuzz`. Reference: [docs/voice-adapter-protocol.md](docs/voice-adapter-protocol.md);
  design: `docs/superpowers/specs/2026-11-04-voice-adapter-design.md`. The bridge's
  adapter client and the test adapter follow; the built-in speech providers are then
  removed (both done: see the entries above and Removed).

- **Agent bridge answering and calling policies** (phase 2, the voice feature's last
  part): `calls.answer: "ask"` makes push-agent-bridge post a card "📞 Owner is calling
  <agent> in <folder> — answer?" (Answer / Decline) on the owner's devices when the owner
  calls, and answer only once "Answer" comes back before the ring ends (declined, missed
  or cancelled calls close the card saying so); `auto` stays the default. The new
  `calls.notify_by_call` section (off by default: `after_minutes` 0 to 8, `urgent`,
  `min_interval_minutes` 10 to 1440, default 10, and `do_not_disturb` local time ranges
  such as `"22:00-07:00"` or `"mon-fri 22:00-07:00"`) lets the bridge call its owner when
  a permission request, question or plan has waited `after_minutes`, or when Coda marks a
  `notify_user` message urgent: the call opens with who calls and why, then the card's
  question, which can be answered by voice. A missed, declined or unreachable call leaves
  the card open and posts a short note; each request calls at most once; a call still
  ringing is cancelled when the card is answered on screen. The control channel gains a
  content-free `call` in `status`, a `call_changed` event and the requests `call_state`,
  `mute` and `hangup` (error `no_call`); the tray shows each agent's call (ringing,
  connecting, in a call, speaking or listening, muted) in the menu line and the window's
  Overview, with **Mute agent** / **Unmute agent** and **Hang up** buttons. Field-named
  validation errors; tests for the policies (paused time), the protocol's content-free
  classification, the tray's view models, and a second end-to-end test in the
  `bridge-voice` job (ask mode, the agent calling its owner who approves by voice, a
  missed call falling back to the card). Guide: [agent bridge
  §18](docs/agent-bridge.md#18-voice-calls).

- **Agent bridge voice calls** (phase 2's bridge voice pipeline): with the new feature
  `voice` (`cargo build --release -p push-bridge --features voice`: push-sdk's media
  helper, Rust 1.89 and CMake) and a `calls` section in `config.json`, push-agent-bridge
  answers its owner's calls (`calls.answer` `auto`; anyone else is refused, a second call
  is declined busy; without `calls`, with `calls.stt` `none` or without the feature, calls
  are not advertised). An in-house energy voice activity detector (`calls.vad`:
  `silence_ms` 700, `min_speech_ms` 300) cuts the owner's speech into utterances;
  speech-to-text turns each into an `agent.transcript@1` message and a prompt, or a steer
  while a turn runs (never a `/` command); the agent's Markdown replies are spoken
  sentence by sentence as they stream (code blocks and tables are said to be in the chat),
  with a greeting, spoken background messages and activity labels `speaking`,
  `listening`, `thinking`, `on a call`. Speaking over the agent stops it at once
  (barge-in) and steers the utterance in. Open cards are read out and can be answered by
  voice (approve or reject, an option by number, ordinal or label, several for a
  multi-select, free text; unclear answers are asked again); the card is then edited into
  a closed "🎤 Answered by voice: …" reply. Providers: `azure-speech` (REST, a key from an
  environment variable with `region`, or an Entra token from a command with `endpoint`),
  `local` (a whisper.cpp server or command for speech-to-text, a command such as piper or
  a URL for text-to-speech) and `none`; logs stay content-free. `status` prints a `Voice:`
  line. Tone-based speech fakes and recorded fixtures test the core, and a new CI job
  `bridge-voice` runs the bridge answering its owner's call over real media through the
  stack; `calls-media-windows` also runs the bridge's voice tests with the feature;
  `PUSH_BRIDGE_LIVE_SPEECH=1` tries real providers. Guide:
  [docs/agent-bridge.md §18](docs/agent-bridge.md#18-voice-calls). (The providers, the
  detector, `calls.stt`/`calls.tts`/`calls.vad`, the fakes and the live speech test were
  replaced by the voice adapter before a release: see Changed and Removed.)
- **Built-in `agent.transcript@1`** (`speaker`: `owner` or `agent`, `text` up to 4,000
  characters; heading "🎤 voice"): a voice transcript, sent by agents. message-svc seeds
  it as approved at its next start, like the other built-ins
  ([docs/rich-messages.md](docs/rich-messages.md#agenttranscript1)).
- **push-sdk media helper** (feature `calls-media`, off by default): `push_sdk::media::MediaCall`
  places or answers a call through `Calls` and runs its audio natively (str0m for ICE,
  DTLS-SRTP and RTP; Opus through libopus; TURN over UDP or TCP through
  `turn-client-proto`, relay-only when the ICE servers include a relay). Apps read and
  write 16 kHz mono 16-bit PCM in 20 ms frames (`incoming() -> PcmStream`,
  `outgoing() -> PcmSink` with `clear()` for barge-in), forward call events to
  `handle()`, and learn the end from `on_ended()` (`MediaEnded` with an `EndCause`).
  The feature needs Rust 1.89 and CMake; push-sdk without it keeps Rust 1.85. A new CI
  job `calls-media` runs two SDK clients with the helper through the stack, once direct
  and twice relay-only through the chart's coturn (UDP and TCP), exchanging tones.
  Guide: [docs/calls.md §19](docs/calls.md#19-media-helper).
- **push-sdk calls API**: `Config::calls` (opt-in; advertises `CAPABILITY_CALLS`) and
  `Client::calls() -> Calls` with `available()`, `invite` (the client picks the UUIDv7
  `CallId`), `accept`, `decline`, `cancel`, `hangup`, `send_ice` (with
  `IceCandidate::end_of_candidates()`), `renegotiate`, `ice_servers()`,
  `history(before, since, limit)` and `hide`; every call frame arrives as
  `Event::Call(CallEvent)` (`Incoming`, `Accepted`, `Taken`, `Ended`, `PeerIce`,
  `PeerRenegotiate`, `Error` with a typed `CallErrorCode`, `Synced` with the calls gone
  since the last connection, `HistoryChanged`). `CallId` joins push-domain's ids. The
  bindings carry the events in JSON (`{"type": "call", …}`); their calls methods follow.
  push-load's calls scenario can report Redis `INFO commandstats` deltas
  (`--redis-url-file`). Guide: [docs/calls.md §18](docs/calls.md#18-push-sdk).
- **Voice calls: client guide, browser recipe and tests** (PR 5):
  [docs/calls.md](docs/calls.md) is now a complete client guide: a field reference for
  every call frame, sequence diagrams for every ending (declined, cancelled, missed, busy,
  unreachable, taken on another device, failed after the disconnect grace, reconnect with
  `CallSync` and an ICE restart, renegotiation glare), offer and perfect-negotiation rules,
  every chart validation message, troubleshooting (no audio, one-way audio, symmetric NAT,
  firewalls and `turns:` on 443), an interoperability checklist (Chromium, Edge, Firefox,
  Safari), and release notes with an upgrade and rollback guide. A reference browser
  client in [docs/examples/calls](docs/examples/calls) (`RTCPeerConnection` with the
  ice-servers API, trickle ICE, `CallSync` handling) with a Playwright smoke test: two
  Chromium contexts with fake media call each other through `stun-only` and, relay-only,
  through the chart's pinned coturn; CI job `browser-calls`. `push-load calls` places
  concurrent calls between prepared pairs and reports setup latency (invite →
  `CallAccepted`) against the targets (< 300 ms p95; 1,000 concurrent calls per gateway
  replica); `prepare --sample-pairs` now goes up to 2000 (`run` still sends sample
  messages on at most 50 pairs). `frame.proto` call comments clarified (no wire change).

- **Voice call relays (TURN)**: `GET /v1/calls/ice-servers` on auth-svc (access token)
  answers the browser-ready ICE servers with short-lived credentials,
  `{"ice_servers": [{urls, username?, credential?}], "expires_at", "relay"}`, or
  `503 turn_unavailable` instead of servers that could not work; 60 requests per user per
  hour. Chart values `calls.turn.*` choose the relay: `internal` (default) deploys a
  hardened coturn (`coturn/coturn:4.18.0-alpine` pinned by digest; TURN REST credentials
  `<expiry>:<user id>` / `base64(HMAC-SHA1(turn-shared-secret, username))`; its own
  UDP+TCP LoadBalancer Service with the relay port range; `denied-peer-ip` for RFC 1918,
  CGNAT, loopback, link-local and cloud metadata addresses, their IPv6 equivalents and the
  required `calls.turn.internal.deniedPeerCidrs`; per-user and total quotas and a
  bandwidth cap; probes; a NetworkPolicy; Prometheus on 9641), `external` uses a generic
  TURN REST provider, Cloudflare Realtime TURN or Twilio (minted through their APIs and
  kept per user until shortly before expiry), `stun-only` is for development. New Secret
  keys `turn-shared-secret` (generated for a chart-managed Secret in `internal` mode),
  `turn-cloudflare-token` and `turn-twilio-token`; external providers need a
  consumer-managed Secret or `secrets.volume`. auth-svc serves
  `push_turn_credentials_issued_total`, `push_turn_credentials_failures_total` and
  `push_turn_provider_requests_total` at `GET /metrics` on its health port. CI scans the coturn image with Trivy. Note: with
  `calls.enabled: true` the default `internal` mode now requires
  `calls.turn.internal.publicHost`, a public IP and `deniedPeerCidrs` (or another mode).
  Guide: [docs/calls.md §13](docs/calls.md#13-turn).

- **Call history** (voice calls, PR 2): every participant of an ended call gets a row in
  their own call history, kept apart from conversations (no "missed call" messages).
  auth-svc serves `GET /v1/calls?before=<cursor>&since=<cursor>&limit=50` (the caller's
  own calls, newest first: `call_id`, `peer_user_id`, `direction` `in`/`out`,
  `started_at`, `answered_at?`, `ended_at`, `duration_s`, `outcome` as `CallEnded.reason`,
  and an opaque `cursor`; `next` when the page is full) and `DELETE /v1/calls/{call_id}`
  (hides the call from the caller's history only; always 204; audited as
  `calls.history.hidden`), rate limited at 120 per minute per user. New server frame
  `CallHistoryChanged` 52 (`latest_ended_at`) tells a user's calls-active devices to
  refresh, for missed-call badges with `since`. message-svc writes the rows when a call
  ends (busy and unreachable invites too; unreachable only for the caller), serves them
  over the new internal `Calls` gRPC service on its Directory listener
  (`Notification.call` gains `history_changed`), and deletes rows older than
  `calls.historyDays` (default 90, 1 to 3650; `PUSH_CALLS_HISTORY_DAYS`) every hour, also
  with calls disabled. Migration `0008_call_history.sql` adds the table
  `messaging.call_history` (no existing data changes). New metrics
  `push_call_history_rows_deleted_total` and `push_call_history_write_failures_total`
  (served at `/metrics` even with calls disabled, so the body is no longer empty then).
  The auth Ingress routes `/v1/calls`. Guide: [docs/calls.md](docs/calls.md#12-call-history).
- **Voice calls (signalling)**: 1:1 WebRTC audio calls between accepted contacts (and
  between an owned agent and its owner) are signalled over the `push.v1` WebSocket; audio
  goes peer to peer and the server never sees it. New frames: client `CallInvite` 11,
  `CallAccept` 12, `CallDecline` 13, `CallCancel` 14, `CallIce` 15, `CallHangup` 16,
  `CallRenegotiate` 17; server `CallIncoming` 44, `CallAccepted` 45, `CallTaken` 46,
  `CallEnded` 47, `CallError` 48, `CallSync` 49, `peer_call_ice` 50,
  `peer_call_renegotiate` 51. Clients advertise `CAPABILITY_CALLS` in `Identify` and the
  server offers it in `Ready`; only connections with both ring or may send call frames
  (others get `CallError NOT_AVAILABLE`). New error codes `ALREADY_ANSWERED` 27,
  `CALL_NOT_FOUND` 28, `IN_CALL` 29 and `CALL_LIMIT` 30. message-svc runs the call engine
  (contact and agent policy, call state in Redis shared by every replica, ring and
  disconnect timers, `call.started`/`call.ended` audit without SDP or candidates) behind
  the new internal `Messaging.Call`, `Messaging.CallAttach` and `Messaging.CallDetach`
  RPCs (`IdentifyResponse.capabilities`, `Notification.call`); the gateway relays the
  frames to the right devices. Chart values `calls.enabled` (off by default),
  `calls.ringSeconds` (45), `calls.disconnectGraceSeconds` (30) and
  `calls.attemptsPerHour` (30) render `PUSH_CALLS_ENABLED`, `PUSH_CALLS_RING_SECONDS`,
  `PUSH_CALLS_DISCONNECT_GRACE_SECONDS` and `PUSH_CALLS_ATTEMPTS_PER_HOUR`. message-svc
  serves the call metrics in Prometheus text at `GET /metrics` on its health port.
  Rolling upgrade: enable `calls.enabled` only after every gateway and message-svc replica
  runs this version. TURN credentials and the SDK API are coming. Guide:
  [docs/calls.md](docs/calls.md).
- **Agent tray v2**: the tray icon is the app's glyph (two linked chat bubbles, `#1b1b1b`
  on a light taskbar, `#ffffff` on a dark one) with a status badge for the most urgent
  agent: green idle, blue working, blue pulsing for starting/stopping/Coda
  restarting/reconnecting (steady with "Reduce motion"), amber waiting for you, purple
  paused, grey no control connection, red stopped/failed/not set up. The SVG lives in
  `crates/push-tray-core/assets/glyph.svg` and is rendered in pure Rust; the app icon
  (`icon.ico`, `icon.png`) is generated by the build. One management window replaces the
  details windows: an agent list, and per agent Overview (state, actions with inline
  refusals, details, last 50 states), Chat, Logs (live tail of `bridge.log`, formatted or
  raw, level filter, search, copy) and Settings (autostart, chat peek, messages kept,
  `config.json` path, remove from tray). The menu is slim: a line per agent, Open…, Add
  agent folder…, Start at logon, Switch from Scheduled Task…, Quit. New `tray.json`
  fields: `show_peek`, `peek_messages`, `menu_peek`, `reduce_motion`, `window`. Guide:
  [docs/agent-bridge.md](docs/agent-bridge.md#13-the-tray-app-push-agent-tray).
- **Agent bridge chat peek**: the bridge keeps the owner conversation's last 50 messages
  (owner texts and answers, replies, cards with their choices and state, one line per tool
  step, notes) in memory only, masked as sent and cut to 300 characters, and serves them
  with the control channel's new `peek {limit?}` request and `peek_changed` event. It is
  the only content on the channel; `"peek": false` in `config.json` turns it off
  (`peek_disabled`). Guide: [docs/agent-bridge.md](docs/agent-bridge.md#chat-peek).
- **Agent bridge control channel**: every `push-agent-bridge run` serves a local,
  per-user channel of newline-delimited JSON (Windows: the named pipe
  `\\.\pipe\push-agent-bridge-<instance id>` with a user-and-SYSTEM DACL and a peer SID
  check both ways; Unix: a 0600 socket under `$XDG_RUNTIME_DIR`): `hello`, content-free
  events (`state_changed`, turns, requests opened and closed, errors), `status`, and the
  actions `restart_backend` (restart Coda, the chat stays connected), `reconnect`,
  `shutdown` (open cards declined: "Declined: the bridge was stopped."), `pause_backend`
  and `resume_backend`. `"control": false` turns it off. New crate `push-bridge-control`.
  Guide: [docs/agent-bridge.md](docs/agent-bridge.md#14-control-channel-for-other-tools).
- **Agent bridge upgrade mode**: `pause_backend` lets Coda drain and save its state
  (`session/prepareRestart`, a fallback for Coda without it), stops it so its program can be
  replaced, holds the owner's texts, watches the program file and starts Coda again with
  `restore` once it changed, announcing what came back ("Coda upgraded 0.1.176 → 0.1.177:
  2 schedules, 1 message and the model/goal restored; …") and asking again to turn
  approvals off. A Coda that stops while its program is missing or locked waits for it
  instead of burning restarts. CLI: `push-agent-bridge pause [--reason <text>]
  [--drain-timeout <seconds>]` and `resume`, so `push-agent-bridge pause; dotnet tool update
  -g coda; push-agent-bridge resume` upgrades Coda without losing the chat. Guide:
  [docs/agent-bridge.md](docs/agent-bridge.md#15-upgrade-mode).
- **push-agent-tray**: a Tauri v2 tray app (`apps/push-agent-tray`, Windows first) that
  supervises the user's bridges in place of the Scheduled Task: an icon coloured by the most
  urgent agent's state, a menu per agent (Start, Stop, Restart Coda, Reconnect, Pause Coda
  for upgrade, Resume, Open log, Open folder, Details…), restarts with backoff (giving up
  after 5 exits in 10 minutes; exit 2 is "Not set up"), "Start at logon" through the
  `HKCU\…\Run` key, `tray.json`, and a switch-over from the Scheduled Task (never both).
  Strict CSP, bundled page only, no permissions for the details window, no network
  listener. Its testable logic is the new crate `push-tray-core`. Guide:
  [docs/agent-bridge.md](docs/agent-bridge.md#13-the-tray-app-push-agent-tray).
- **Safe Markdown in templates**: a tpl/1 `markdown` block (`{"type": "markdown", "param":
  "<multiline string>"}`, no literal, no label) whose value the server checks against a
  CommonMark subset: paragraphs, emphasis, strikethrough, code spans, fenced and indented
  code, headings 1–6, lists nested up to 4 and quotes up to 3 levels, GFM tables up to 20
  columns and 200 rows, thematic breaks, and links and autolinks to absolute http(s) URLs
  only. Raw HTML, images, footnotes and any other link scheme (`javascript:`, `data:`,
  `file:`, `mailto:`, …, also in link reference definitions) are refused with
  `ERROR_CODE_INVALID` and the reason. The subset and how clients must render it (from a
  Markdown AST into native elements, links with their URL shown and confirmed, no images
  or remote content, code never run) are in
  [docs/rich-messages.md](docs/rich-messages.md#markdown).
- **`agent.reply@2`** built-in: `agent.reply@1` with `text` rendered as Markdown. It is
  seeded at start like the other built-ins; `@1` stays. Clients that do not know it show
  the fallback.
- **Agent bridge**: `/yolo`, the same as `/mode bypass` (the same confirmation card,
  reset and `/status` line). The `Backend` trait declares its `TextFormat` (plain by
  default); Coda writes Markdown, so its turns and background messages are sent as
  `agent.reply@2` with the Markdown kept and masked, a marker-free fallback, and pages
  that never end inside a code block. The bridge's own messages stay `agent.reply@1`.

- **Agent bridge with a Coda backend**: `push-agent-bridge` (new crate `push-bridge`)
  reaches a local AI coding agent from push-messenger as a private agent contact. Guide:
  [docs/agent-bridge.md](docs/agent-bridge.md) (setup on Windows, commands, security
  model, troubleshooting, live smoke checklist).
  - A backend-independent core: agent-key sign-in with the OS key, the owner gate ("This
    agent is private." to anyone else, at most once per sender per hour), one
    `agent.reply@1` per turn (thinking, up to 50 steps, text paged at 4,000 characters,
    edits at most every 2 s, "(stopped)"), cards for the agent's requests with the
    owner's answers checked by the bridge and a 10-minute deadline ("No answer;
    skipped"), activity (working, waiting, idle), secret masking and caps on everything
    sent, the owner's `/` commands (`/new`, `/cd` inside configured roots, `/stop`,
    `/status`, `/mode bypass` behind a confirmation card, …), sessions per folder, and
    restarts with backoff (1 s to 60 s). A small `Backend` trait with typed events and
    capabilities; a `TestBackend` runs the core without Coda.
  - The Coda backend drives `coda serve` (JSON-RPC 2.0 over stdio): prompts, steering,
    permission, question and plan requests, background messages and task completions,
    and Coda's session, model, effort, mode, goal, task, schedule and skill commands. It
    never sets `CODA_SERVE_API_KEY`.
  - `config.json`, `state.json` (no secrets) and a JSON-lines log without bodies or
    tokens in `%LOCALAPPDATA%\push-messenger\coda-bridge` (or `--dir`); commands `keygen`,
    `thumbprint`, `enroll --code`, `run`, `status`; `scripts\install.ps1` and
    `uninstall.ps1` register and remove a logon Scheduled Task that restarts on failure.
- **push-sdk: agent-key sign-in through a signer**
  ([guide](docs/agent-key-sign-in.md#from-push-sdk-rust)).
  `Client::enroll_with_agent_key` and `Client::sign_in_with_agent_key` take an
  `AgentSigner` the app supplies, so the private key stays in OS key storage: the SDK
  builds the claims (`aud`, `iat`, `exp = iat + 120`, a new `jti` for every request) and
  the signer returns the compact JWS. Helpers: `AgentClaims`, `AgentPublicKey::thumbprint`
  (RFC 7638), `assemble_assertion`. A failing signer is the new `SdkError::Signer`, code
  `signer` (not retryable; the key provider's code, never key material). Rust only: the C
  and Node bindings do not expose it yet.
- **push-sdk: `CardBook`**, which checks answers to the cards an app sent: from the card's
  owner, in its conversation, naming the card, with offered values only, before its
  deadline (the server does not check who answers). Rust only.
- **`push-agent-key`** (new crate): agent-key signers. `cng::CngKey` keeps a persisted,
  non-exportable P-256 key in Windows key storage, in the TPM (Microsoft Platform Crypto
  Provider) when it can and otherwise in the Microsoft Software Key Storage Provider;
  `software::SoftwareKey` keeps the key in a PKCS#8 file, for tests and development only.
- **Built-in templates `agent.choose@1`** (multiple choice, 1 to 10 of up to 10 options)
  **and `agent.plan-approval@1`** (a plan of up to 4,000 characters with Approve plan and
  Reject), for owned agents ([definitions](docs/rich-messages.md#6-built-in-templates)).
  message-svc seeds them as approved at its next start; nothing else to do.
- **Live activity and presence** (REQUEST PUSH feature 12). Guide:
  [docs/activity-presence.md](docs/activity-presence.md) (frames, rules, multi-replica
  design, agent recipe).
  - Activity: a new client frame `Activity` (10) says what the user is doing in a
    conversation (`typing`, `working`, `waiting`, `idle`, with an optional plain-text label
    of 1 to 60 characters; empty means none). The other current members' connected devices
    hear it as `ActivityEvent` (42) with `ttl_ms` 30000 (0 for idle), to expire locally at
    receive time + ttl; the sender's own devices and members with a block either way do
    not. Never stored. Paced per device (burst 2, then 1 per second; extra frames are not
    refused, the latest per conversation is sent when a token frees up); a closed
    connection (not a takeover) sends idle for its live activities. Refusals are `Error`
    frames without ids that are never retryable.
  - Presence: `PresenceEvent` (43) pushes a contact's `ONLINE`/`OFFLINE` (with a whole
    `last_seen` minute) on change, and the request `GetPresence` (20, answered by
    `PresenceList` 11) reads 1 to 100 users, rate limited to 120 per minute per user.
    Visible only between accepted contacts; everything else (strangers, blocks, opt-outs,
    unknown ids) reads `UNKNOWN`, indistinguishably. Online while any connection on any
    gateway replica holds a lease in Redis (90 s, renewed every 30 s); a closed
    connection keeps it for a grace (`PUSH_PRESENCE_GRACE_MS`). Blocking an accepted
    contact pushes `UNKNOWN` both ways.
  - Opt-out: `SetPresenceSharing` (21, answered by `PresenceSharing` 12; without a value it
    reads the setting) switches sharing off or on; reciprocal (a user who hides sees
    everyone as `UNKNOWN`).
  - Internal: `Messaging.Activity` and `Messaging.AnnouncePresence` gRPC calls and
    `Notification.activity_changed`/`presence_changed`; Redis keys
    `presence:{<user>}:conns` and `presence:{<user>}:state`.
  - SDK and bindings: `send_activity`, `get_presence`, `set_presence_sharing`,
    `presence_sharing` (Node `sendActivity`, `getPresence`, `setPresenceSharing`,
    `presenceSharing`; C `push_client_send_activity`, `push_client_get_presence`,
    `push_client_set_presence_sharing`, `push_client_presence_sharing`) and the events
    `ActivityChanged`/`PresenceChanged` (`activityChanged`/`presenceChanged`, with
    millisecond fields `expiresAtMs` and `lastSeenMs`).
  - Helm: `gateway.presenceGraceMs` renders `PUSH_PRESENCE_GRACE_MS`.

  **Upgrade notes:**
  - messaging migration `0007_presence_hidden.sql` adds the table
    `messaging.presence_hidden` (the opt-out); it changes no existing data and runs with
    the usual messaging migrations. Rolling message-svc back after it is not supported,
    as for 0005.
  - Deploy message-svc before the gateways. A new gateway in front of an old message-svc
    answers `Activity` with `UNAVAILABLE`, gets `INVALID` for the presence requests and
    cannot announce presence; old gateways ignore the new notifications.
  - New gateway variable `PUSH_PRESENCE_GRACE_MS`: 0 to 120000 milliseconds, default
    10000. No new secret: presence uses the Redis the gateways and message-svc share.
  - Additive protocol: older SDKs log the new server frames as unreadable and ignore them.
- **Rich message templates and a template marketplace** (REQUEST PUSH feature 8). Two new
  content types: `CONTENT_TYPE_RICH` (3), a card filled from an approved tpl/1 template
  (`{tpl, params, fallback}`, up to 32 KiB), and `CONTENT_TYPE_ANSWER` (4), an answer to a
  card (`{card, values, fallback}`, up to 4 KiB). message-svc enforces everything, since
  bodies are not end-to-end encrypted: the template's approval, the full params schema,
  the sender's kind (`agent` for owned agents, `human` otherwise) and every answer. The
  first valid answer closes the card: the new `Deliver.answer_seq` carries it, the
  answer's `Deliver.target_seq` is the card's seq, and later answers get
  `ERROR_CODE_REJECTED`; answered cards and answers cannot be edited. Refused bodies are
  `ERROR_CODE_INVALID`; no new frame or error code. Guide:
  [docs/rich-messages.md](docs/rich-messages.md) (tpl/1 reference, review checklist,
  catalog protocol).
- auth-svc: `POST /v1/templates` submits a definition for review (access token, JSON up
  to 16 KiB; publishers are claimed by the first submitter, `agent`, `push`, `system`,
  `admin`, `builtin`, `official`, `verified` and every publisher starting with one of
  them are reserved, at most 20 pending per account);
  `GET /v1/templates?since=N` serves the client catalog of approved and revoked versions
  (pages of 200, `seq`/`more`, ETag `"tpl-<since>-<seq>"` with 304 on `If-None-Match`,
  `Cache-Control: private, no-cache`). The admin listener gains `GET /v1/admin/templates`,
  `GET /v1/admin/templates/{ref}` (definition, outline, audit trail) and
  `POST /v1/admin/templates/{ref}/approve|reject|revoke` (`{reviewer, note?}`): holders of
  `PUSH_AUTH_ADMIN_TOKEN` are the reviewers. New error codes `not_found` (404) and
  `conflict` (409); new rate limits: submissions 10 per hour per user and 60 per IP, the
  catalog 120 per minute per user and 600 per IP. New security events
  `auth.template.submitted`, `auth.template.refused` and `auth.admin.template_reviewed`
  (with the reviewer's IP); message-svc logs `templates.submitted`, `templates.approved`,
  `templates.rejected`, `templates.revoked`, `templates.seeded` and
  `templates.builtin_mismatch`.
- Built-in templates `agent.reply@1`, `agent.approval@1`, `agent.question@1` and
  `agent.plan@1` (agents only), seeded as approved when message-svc starts, before it is
  ready; an admin's revocation survives restarts. Designed upstream without the
  consumer's spec: reconcile and propose changes as new versions.
- messaging migrations `0005_templates.sql` and `0006_validate_message_checks.sql`:
  content types 3 and 4, `answer_seq` on messages, the registry (`template_publisher`,
  `template_version`, the `template_change` counter) and the append-only audit trail
  `messaging.template_event`, guarded by triggers (immutable versions, one-way
  statuses). They change no existing data and run with the usual messaging migrations.
  **Upgrade notes:**
  - 0005 is metadata-only: the new `answer_seq` column is nullable without a default and
    the new message CHECKs are added `NOT VALID`, so it holds the message table's
    exclusive lock only for a moment, without a scan (with `lock_timeout` 5 s: behind a
    long transaction it fails and start-up retries it instead of stalling all traffic).
    0006 then validates the CHECKs online: `VALIDATE CONSTRAINT` scans under SHARE UPDATE
    EXCLUSIVE, so reads and writes continue. The scan grows with the message table and
    runs before the pod reports ready: on a large installation run the migrations as a
    job first (`push-message-svc migrate`, the chart's migrate job) or allow a longer
    start-up probe.
  - During a rolling upgrade old message-svc pods cannot decode content types 3 and 4: a
    request that reads a card or an answer fails there. Finish rolling out message-svc
    before approving or sending templates (the built-ins are approved as soon as the
    first new pod starts, so also before agents send cards).
  - Rolling back message-svc after 0005 is not supported: sqlx refuses a database with
    applied migrations the binary does not know (`VersionMissing`), so an older
    message-svc that migrates at start-up (`PUSH_RUN_MIGRATIONS=true`, the default) keeps
    retrying and never becomes ready, and an older `push-message-svc migrate` job fails.
    With `PUSH_RUN_MIGRATIONS=false` an older binary starts but fails every request that
    reads a card or answer.
  - Older SDKs do not know content types 3 and 4: they show RICH and ANSWER messages as
    text, their raw JSON body, and keep them as text after an SDK upgrade (entries
    already received are not reclassified). Ship the updated SDK to your apps before
    agents send cards.
- message-svc serves a new internal gRPC service, `push.internal.v1.Templates`, on the
  Directory port with auth-svc's existing token: **no new configuration** (auth-svc uses
  `PUSH_MESSAGE_SVC_URL` and `PUSH_DIRECTORY_TOKEN`; without them the template routes
  answer 503). The chart's auth Ingress routes `/v1/templates`.
- SDK and bindings: `MessageKind::Rich`/`Answer` (`rich`/`answer`), `answerSeq` on cards
  and `cardSeq` on answers, and `send_rich`/`send_answer` (Node `sendRich`/`sendAnswer`,
  C `push_client_send_rich`/`push_client_send_answer`) with local envelope checks. The
  SDK does not fetch the catalog yet.
- **Agent-key sign-in for owned agents** (REQUEST PUSH feature 6). A new opt-in provider,
  `agent-key` in `PUSH_AUTH_PROVIDERS` (off by default), lets an owned agent sign in with
  a P-256 key pair of its own instead of an Entra app token: no secret, certificate or
  managed identity. The agent sends a single-use ES256 JWS (`typ: push-agent+jwt`, its
  public key in the `jwk` header, `aud`/`iat`/`exp`/`jti` claims, at most 300 s of
  lifetime) to `POST /v1/auth/agent-key`; the first one, with the owner's enrollment code
  (`{token, enrollment_code}`), binds the key's RFC 7638 thumbprint to the reserved agent.
  Then `{token}` and `{token, device_id}` sign in as for `entra-app`. An unenrolled key
  gets 401 `token_invalid` and never creates an account; a reused `jti` is refused across
  replicas. New variables `PUSH_AUTH_AGENT_KEY_AUDIENCE` (default: the public URL without
  a trailing `/`) and `PUSH_AUTH_AGENT_KEY_MAX_SESSION_HOURS` (1 to 720, default 24); new
  events `auth.agent-key.success` and `auth.agent-key.failure`. `POST /v1/agents` now
  also works with `agent-key` alone (the reservation has no tenant, and only a key can
  redeem it). Key rotation is not supported yet: delete the agent and enroll a new one.
  Guide: [docs/agent-key-sign-in.md](docs/agent-key-sign-in.md), with a PowerShell
  example that keeps the key in the TPM.
- `GET /v1/agents` lists two more fields per agent: `credential` (`"entra-app"`,
  `"agent-key"`, or `null` while pending) and `key_thumbprint` (the key's thumbprint for
  `agent-key`, otherwise `null`).
- Helm: `auth.signIn.providers` accepts `agent-key`, and
  `auth.signIn.agentKey.{audience,maxSessionHours}` render the two variables (the
  audience only when set). The chart validates both.
- auth migration `0009_agent_key.sql` makes `auth.agent_enrollment.tenant_id` nullable
  and adds the `auth.agent_key_assertion` replay table. It changes no existing data and
  runs with the usual auth migrations. During a rolling upgrade an old pod fails (500)
  only a redemption of a tenantless reservation, which only a new pod with `agent-key`
  makes.

### Changed

- **Agent bridge commands:** `/new` and `/cd` now need the backend's `Sessions`
  capability like `/sessions` and `/resume`, and `/help` lists only the commands the
  backend supports (Coda: all, as before).
- **push-agent-tray:** the backend's name comes from the bridge's status ("Restart Echo",
  "Echo restarting", "Echo version"; "backend" before the first status) instead of a
  fixed "Coda". The window's agent JSON has `backendName` and `backendVersion` instead of
  `codaVersion`; `AgentState::Restarting` carries the backend's name and
  `AgentAction::label` takes it.
- **Agent bridge: speech runs only in a voice adapter** (third of three steps that move
  speech out of push-agent-bridge). A `config.json` whose `calls` has `stt`, `tts` or
  `vad` (whatever the value, `none` and `null` included) no longer starts: `run` and
  `enroll` exit 1 and `status` shows, for the first of them, `calls.stt is no longer
  supported: speech runs in a voice adapter. Configure calls.adapter (docs/agent-bridge.md
  §18, docs/voice-adapter-protocol.md).` (`calls.tts`, `calls.vad` alike). Calls need
  `calls.adapter`: without it calls are not advertised and `status` prints `Voice: off
  (no calls.adapter)`; `calls.notify_by_call` with `after_minutes` above 0 or `urgent`
  without it is a config error (`calls.notify_by_call needs calls.adapter: without it the
  bridge takes no calls and places none`). The feature `voice` now builds only the media
  helper. The voice tests' recorded fixtures moved from
  `crates/push-bridge/tests/fixtures/voice` to `crates/push-voice-adapter-testkit/fixtures`
  (the testkit's `fixture` module owns their rules), and the `bridge_voice` end-to-end
  test runs through the testkit's adapter. Migration: [agent bridge
  §18](docs/agent-bridge.md#migrating-from-callsstt-and-callstts); writing an adapter:
  [docs/voice-adapter-protocol.md §14](docs/voice-adapter-protocol.md#14-writing-an-adapter).
- **Agent bridge `calls.stt` defaults to `none`** (step two; superseded by the entry
  above before any release: `calls.stt` is now refused), so `"calls": {"adapter": …}`
  alone is a valid calls section; with `calls.adapter`, `calls.stt` and `calls.tts` had to
  be absent or `none` and `calls.vad` absent or default. A `calls` section without
  `calls.adapter` is accepted and advertises no calls.
- **Agent bridge built-in speech providers deprecated** (step two): `calls.stt`,
  `calls.tts` and `calls.vad` (`azure-speech`, `local`, the energy detector) worked as
  before without `calls.adapter`. Step three removed them (see Removed below).

- **Gateway logging**: the gateway never logs WebSocket payloads, even with
  `RUST_LOG=trace`: the WebSocket library (`tungstenite`) is capped at `debug`, since at
  `trace` it logs every frame with its content (access tokens, messages, call SDP and ICE
  candidates). `push_gateway::log_filter` applies the cap to any filter.
- **Agent bridge**: a card ends the reply before it. When a request opens a card, the
  turn's reply gets its final state (a step still running shows as `pending`) and is not
  edited again; what the agent does after the owner's answer goes into a new reply below
  the answer, where the waiting step finishes. Requests in a row post their cards one
  after the other, and a turn that ends right after a card posts no empty reply. See
  [docs/agent-bridge.md](docs/agent-bridge.md#6-behaviour).
- `serve_with` refuses an `Overrides` authenticator with the built-in id `agent-key`, as
  for `entra-app`. An `Overrides` admission is also accepted with `agent-key` alone, and
  the start-up error without any sign-in by token now names all of them (`entra`,
  `entra-app`, `agent-key`, `external`).
- Chart: the email errors now say "without password" instead of naming `entra`, since
  any `auth.signIn.providers` list without `password` writes no email.
- **Deleting an agent releases its app identity** (REQUEST PUSH feature 7). `DELETE
  /v1/agents/{agent_id}` of an active agent still disables it for good and revokes its
  devices and tokens, and now also releases its app registration or managed identity in
  the same transaction: a fresh `POST /v1/auth/entra-app {token, enrollment_code}` with
  that app enrolls a new agent (of any owner) instead of failing with `identity_in_use`.
  Until then the released app gets the normal unenrolled 403 `sign_in_not_permitted`.
  Two live agents still cannot share an app. The released binding is kept in the new
  `auth.released_identity` table (tenant and client app id stay on
  `auth.agent_enrollment`) and logged as `auth.agent.identity_released`. Admin
  revoke-sessions is unchanged and does not release.
- auth migration `0008_released_identity.sql` adds the `auth.released_identity` audit
  table. It changes no existing data and runs with the usual auth migrations.
- **Handles may contain hyphens** (REQUEST PUSH 42). A handle is now 3 to 30 characters
  of `a-z`, `0-9`, `_` and `-`, starting with a letter, not ending with `-` and without
  `--` (so `yury-opolev` and `build-bot` are accepted; `bob-`, `a--b` and `-bob` are not).
  Input is still trimmed, lower-cased and stripped of one leading `@`; lookups and
  uniqueness still ignore case. This applies to signup, Entra and external sign-in,
  `PATCH /v1/users/me`, `GET /v1/users/lookup`, the admin API, `POST /v1/agents` and
  `PATCH /v1/agents/{agent_id}`. The 400 reason is now "use 3 to 30 letters, digits,
  underscores or single hyphens, starting with a letter and not ending with a hyphen".
  **Update client-side handle validation** to match, e.g.
  `^[a-z][a-z0-9_-]{1,28}[a-z0-9_]$` plus "no `--`".
- auth migration `0007_hyphenated_handles.sql` replaces the `auth.user_account` handle
  CHECK. It only widens the rule; no data changes. It runs with the usual auth
  migrations.

### Removed

- **Agent bridge built-in speech providers**: `azure-speech` and `local`, the energy voice
  activity detector, and their settings `calls.stt`, `calls.tts` and `calls.vad` (now a
  startup error, see Changed). Speech runs only in a voice adapter (`calls.adapter`,
  [`push-voice-adapter/1`](docs/voice-adapter-protocol.md)): wrap the engines you used
  (whisper.cpp, piper, Azure AI Speech) in one. Also removed: the opt-in live speech test
  (`crates/push-bridge/tests/live_speech.rs`, `PUSH_BRIDGE_LIVE_SPEECH`; real engines are
  tested in their own adapters), the bridge's tone fakes (replaced by the testkit adapter)
  and push-bridge's `reqwest` dependency (push-sdk still uses it for its HTTP client).

### Fixed

- **Chart: coturn crash-looped with `exec /usr/bin/turnserver: operation not
  permitted`**: with `calls.enabled` and internal TURN the coturn pod never started (so
  `helm upgrade --atomic --wait` timed out and rolled back). The pinned image gives
  `/usr/bin/turnserver` the file capability `cap_net_bind_service=ep`, and the kernel
  refuses to exec such a binary when the capability cannot be granted, as with the
  chart's `capabilities.drop: [ALL]` and `allowPrivilegeEscalation: false`. An init
  container (`turnserver`, same pinned image) now copies the binary with a plain `cp`,
  which leaves the capability behind, into an `emptyDir` at `/opt/coturn/bin`, and coturn
  runs that copy; it listens above 1024 only (3478, 5349, 9641). The security context is
  unchanged (non-root, no capabilities, no privilege escalation, read-only root,
  `RuntimeDefault` seccomp: restricted Pod Security Standard) and there is no new image
  to mirror. `chart-cluster` CI now installs in a namespace enforcing `restricted`, waits
  for coturn to be Ready and runs a TURN allocation against it
  ([docs/calls.md §13.4](docs/calls.md#134-coturn-in-the-chart)).

- **Chart: coturn's NetworkPolicy refused by the API server**: with `calls.enabled` and
  internal TURN, `helm install`/`upgrade` failed on a real cluster
  (`NetworkPolicy "<release>-coturn" is invalid: ... "::ffff:0:0/96": must not have an
  IPv4-mapped IPv6 address`), as Kubernetes refuses IPv4-mapped IPv6 CIDRs in `ipBlock`.
  The policy's egress exceptions now leave out `::ffff:0:0/96` (and anything within) and
  are written in canonical form; coturn's `denied-peer-ip` lines are unchanged, so it
  still denies `::ffff:0:0/96`. `calls.turn.internal.deniedPeerCidrs` now refuses
  IPv4-mapped entries. CI applies the chart to kind clusters of Kubernetes 1.34 to 1.37
  (`chart-cluster`, [docs/calls.md §13.4](docs/calls.md#134-coturn-in-the-chart)).

- **push-sdk media helper on coarse timers**: the helper now sends a frame every 20 ms on
  average where the timer is coarse. On Windows (a ~15.6 ms timer) a late 20 ms tick came
  every ~31 ms, about 32 frames a second, so the peer heard gaps and the audio fell
  behind; late ticks are now caught up, and after a stall over 100 ms the clock starts
  again ([docs/calls.md §19.4](docs/calls.md#194-audio)).