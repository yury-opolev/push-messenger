# Voice adapter (speech moves out of the bridge) — design addendum

Change request "voice adapter" (consumer yuopolev/push-messenger, against upstream
94657f8). An addendum to the [voice calls design](2026-11-03-voice-calls-design.md)
(cited as "voice spec §n"): it replaces the bridge's built-in speech providers (voice spec
§10.3 and §15 "PR 7") with a separate, swappable **voice adapter** process that the bridge
talks to over a local, versioned protocol, `push-voice-adapter/1`. The owner is away:
every open question was decided autonomously and is recorded in [Decisions](#1-decisions).
Delivered as three independently releasable PRs ([plan](../plans/2026-11-04-voice-adapter.md));
PR 3 lands only once PRs 1 and 2 are released. Names and numbers below are final unless a
later PR records a deviation in [§12](#12-implementation-notes).

The wire reference for adapter authors is [docs/voice-adapter-protocol.md](../../voice-adapter-protocol.md);
this document is the contract behind it, plus the bridge-side design.

## Problem

PRs #61–#62 put speech-to-text and text-to-speech inside push-agent-bridge (`--features
voice`, providers `azure-speech` and `local`). push-messenger connects people and agents
and carries their audio; it must not know how speech is recognised or produced. Keeping
STT/TTS in the bridge ties every engine choice to push-messenger releases, puts speech
credentials (Azure keys, Entra tokens) and engine processes inside the bridge's trust
boundary, and grows its dependencies.

## Goals and non-goals

Goals: the bridge answers, routes and carries audio; a voice adapter (any language, any
engine: local, cloud, realtime model) does voice activity detection, recognition and
synthesis; one small, versioned, fuzzed protocol between them with JSON Schemas; spawn
and connect modes; graceful fallback to chat-only when the adapter is gone; every voice
behaviour of PRs #61–#62 kept, through the adapter.

Non-goals (v1): several calls per adapter connection (call_id is on every message so v2
can multiplex), audio formats other than 16 kHz mono s16le 20 ms (negotiated in a later
version, never assumed), a network transport (local only), an adapter shipped as a
product (the testkit adapter is for tests; ours lives in yuopolev/voice-adapter).

## 1. Decisions

| # | Topic | Decision |
|---|---|---|
| A1 | Crates | PR 1: `crates/push-voice-adapter-proto` (framing, messages, limits, version negotiation, JSON Schemas), MSRV 1.85, no dependency on the media stack; tokio support behind the default feature `tokio`. PR 2: `crates/push-voice-adapter-testkit` (a scripted adapter binary + library). The bridge's client lives in push-bridge (`voice/adapter/`). |
| A2 | Frame length | The u32 big-endian length counts the **payload only** (not the kind byte, not itself): a frame is `len:u32be kind:u8 payload[len]`. `len ≤ 65 536` for both kinds; a larger length is refused from the header alone, before anything is buffered. |
| A3 | Frame kinds | 1 = JSON control, 2 = audio. Any other kind is a framing error in v1 (fatal: the connection closes); a new kind only ever comes with a new negotiated protocol version. |
| A4 | Audio header | 9 bytes: `stream:u8 seq:u32be timestamp_ms:u32be`, then the samples as s16le (an even, non-zero number of bytes). Streams: 0 = from the caller (bridge → adapter), 1 = to the caller (adapter → bridge); any other stream is a framing error. The codec is format-agnostic; the v1 format (320 samples per frame) is checked by the receiver with `AudioFormat::check`. |
| A5 | Numbers | Big-endian for the framing and audio header (network order), little-endian for samples (the PCM convention every engine emits). |
| A6 | Version negotiation | The bridge's hello names its preferred `protocol` and may list every version it speaks in `protocols` (additive field; absent = only `protocol`). The adapter answers with one of them; if it speaks none it sends `error unsupported_protocol` and closes. A bridge receiving a version it did not offer sends `unsupported_protocol` and closes. Versions are `push-voice-adapter/<n>`; the highest common `n` wins. |
| A7 | Unknown things | Unknown JSON fields are ignored; unknown control `type`s are ignored (logged by type name, sanitized); new types and fields are only ever added. Unknown enum values (`call_ended.reason`, `error.code`) decode as `other`. A known type whose required field is missing or invalid is answered with `error invalid_message` and dropped (not fatal), except a hello (fatal). |
| A8 | Ids | `call_id` and `speak_id` are chosen by the bridge, `utterance_id` by the adapter: 1–64 printable ASCII characters (0x21–0x7E), opaque. The bridge's `call_id` is a fresh random id per call, **not** the server's call id: nothing the adapter sees links to push-messenger state. |
| A9 | Speak queue | The adapter speaks `speak` requests strictly in the order received, one at a time; stream-1 audio belongs to the oldest unfinished speak. Every `speak` gets exactly one `speak_done` (`interrupted: true` when stopped or dropped before or while playing, also on synthesis failure after an `error synthesis_failed`). The bridge keeps at most 16 speaks unfinished and queues the rest itself. |
| A10 | Barge-in | On `speech_started` the bridge clears its playback buffer, mutes the running reply (voice spec §15 PR 7 "session mute") and sends `stop_speaking` (no `speak_id`: everything). An adapter with `barge_in: true` may already have stopped itself; `stop_speaking` with nothing to stop is a no-op. Without `vad` there is no `speech_started`: the bridge stops speech when a final transcript arrives. |
| A11 | Liveness | Each side sends `ping {id}` every 10 s and answers every ping with `pong {id}` (same id). Any frame received counts as life; 30 s without one is a dead peer (close). The adapter's hello must arrive within 10 s of the bridge's. |
| A12 | Backpressure | Bridge → adapter audio: a 1 s (50-frame) send queue; when full the oldest audio frame is dropped and counted (`seq` gaps tell the adapter); control messages are never dropped and go first. Adapter → bridge audio: played at real time from a buffer of at most 2 s (100 frames); frames beyond are dropped and counted. A peer that does not drain its side for 30 s is dead (A11). |
| A13 | Rate caps | The bridge accepts at most 100 control messages per second from the adapter (a 1 s window; partial transcripts beyond 10 per second are dropped first); exceeding it is `error overloaded` and the connection closes. Stream-1 audio at more than 4× real time over a 2 s window: frames dropped (A12). |
| A14 | Text caps | `transcript.text` and `speak.text` 1–4 000 characters (Unicode scalar values); `error.message` ≤ 512; names, versions, voices ≤ 64; language ≤ 35 (BCP 47 shape `[A-Za-z0-9-]`); at most 256 voices. |
| A15 | Content-free | Transcripts, speak text, the peer's display name and audio never reach a log, metric, the control channel or an error message. The crate's `Debug` impls print lengths instead; decode errors never echo input (they carry the message type, a category and a position). |
| A16 | Schemas | Generated with `schemars` 1.x (MIT, already in Cargo.lock; builds on 1.85) from the Rust types — one file per message plus the two unions, checked in under `docs/schemas/voice-adapter/v1/`. A test regenerates them and fails on any difference (`UPDATE_VOICE_ADAPTER_SCHEMAS=1` rewrites). Rejected: hand-written schemas with a validator crate in tests (a large dev-dependency tree and two sources of truth). |
| A17 | Fuzzing | A cargo-fuzz target (`crates/push-voice-adapter-proto/fuzz`, its own workspace, not built by `cargo build --workspace`) for the frame decoder and both message decoders, run for 60 s by a CI job `voice-adapter-fuzz` on nightly; plus proptest "fuzz-like" tests in the crate's ordinary tests (random bytes, random splits). |
| A18 | Spawn | `calls.adapter.spawn.command`: argv list (≥ 1 element, the program an absolute path or found on PATH by the OS's process creation, never a shell), no window (Windows `CREATE_NO_WINDOW`), stdin/stdout piped for the protocol, stderr to `<state dir>/logs/voice-adapter.log` (rotated at 5 MiB, 1 old file) or discarded with `"stderr": "discard"`. Environment: the bridge's own minus every variable it treats as secret (`CODA_SERVE_API_KEY`, the variables named by the bridge's config as secret sources, `PUSH_*` credentials) — a deny-list plus an optional `env` allow-list override. Killed with the bridge (kill-on-drop, Windows job object). Restarted with the backend's backoff (1 s doubling to 60 s, reset after 60 s up). |
| A19 | Connect | `calls.adapter.connect`: `{"pipe": "<name>"}` → `\\.\pipe\<name>` (Windows), `{"socket": "<path>"}` (Unix, `$XDG_RUNTIME_DIR` expanded). The **adapter** listens, the bridge connects with push-bridge-control's transport client (`transport::connect`), which checks the server the way the control channel does: same user SID or SYSTEM (Windows), the socket's owner uid (Unix). Reconnects with the same backoff. |
| A20 | Advertising | Calls are advertised only while an adapter is ready (hello done) with `stt: true`. push-sdk's calls capability is fixed per connection (`Identify`), so PR 2 adds `Client::set_calls_advertised(bool)` which re-identifies (a reconnect, covered by `CallSync`, voice spec D7) when the value changes. Never during a call: a change waits until no call is ringing, answering or running; flapping is damped (an adapter must stay ready 2 s before calls are advertised; withdrawal is immediate once no call runs). |
| A21 | Mid-call loss | The adapter going away mid-call keeps the call up (the media is the bridge's): the bridge posts "Voice adapter stopped; reconnecting…" (an `agent.reply@1` note), switches the call to chat-only (no speech; replies stay in the chat; the owner's audio is dropped), and when a new adapter is ready during the same call sends it `call_started` again with the same `call_id` and posts "Voice adapter is back." |
| A22 | Language | `calls.language` (BCP 47, default `en-US`) goes in the bridge's hello and in each `call_started`. |
| A23 | Partials | Partial transcripts (`final: false`) only set the activity to "listening"; they are never posted, prompted or matched. Only finals act. |

## 2. Architecture

```
            owner's device                          agent's computer (same user)
  ┌───────────────────────┐   WebRTC (Opus)   ┌──────────────────────────────────────────┐
  │ web app / push-sdk    │◀─────────────────▶│ push-agent-bridge                        │
  └───────────────────────┘                   │  media helper (push-sdk calls-media)     │
                                              │   PCM 16 kHz mono s16le, 20 ms frames    │
                                              │  core: answer policy, owner rules, DND,  │
                                              │   prompt/steer, card matching, masking,  │
                                              │   speech text, activity, transcripts,    │
                                              │   tray call state, mute, hang up         │
                                              │  voice/adapter: client, router, fallback │
                                              └──────────────┬───────────────────────────┘
                                                stdin/stdout │ or named pipe / Unix socket
                                                 push-voice-adapter/1 frames
                                              ┌──────────────▼───────────────────────────┐
                                              │ voice adapter (any language, any engine) │
                                              │  VAD · STT · TTS · its own credentials   │
                                              └──────────────────────────────────────────┘
```

## 3. Protocol `push-voice-adapter/1`

### 3.1 Transport

A reliable, ordered byte stream between exactly one bridge and one adapter: the adapter
child's stdin (bridge → adapter) and stdout (adapter → bridge) in spawn mode, or one
connection to the adapter's named pipe / Unix socket in connect mode. Either side closing
the stream ends the session; a side that hits a fatal error sends an `error` first when it
can, then closes.

### 3.2 Framing

```
 0               4     5                                   5+len
 ├───────────────┼─────┼───────────────────────────────────┤
 │ len (u32, BE) │kind │ payload (len bytes)               │
 └───────────────┴─────┴───────────────────────────────────┘
 kind 1: payload = one JSON object, UTF-8, no BOM          (len 1..=65 536)
 kind 2: payload = audio header (9 bytes) + samples s16le  (len 11..=65 536, len-9 even)
```

- `len` counts the payload only. `len > 65 536` is a framing error, detected from the
  5-byte header before any payload is read or buffered.
- Frames follow each other with nothing in between; a reader must accept frames split
  across reads and several frames in one read.
- A stream that ends inside a frame is an error (`unexpected end`); one that ends on a
  frame boundary is a clean close.
- Framing errors (bad kind, bad stream, oversize, a malformed audio payload) are fatal: the
  receiver sends `error invalid_frame` when it can and closes. JSON errors inside a valid
  kind-1 frame are not framing errors (see A7).

### 3.3 Audio frames

| Offset | Size | Field | Meaning |
|---|---|---|---|
| 0 | 1 | `stream` | 0 = from the caller (bridge → adapter), 1 = to the caller (adapter → bridge) |
| 1 | 4 | `seq` | u32 BE. Per stream and call, starts at 0, +1 per frame, wraps. A gap = frames dropped. |
| 5 | 4 | `timestamp_ms` | u32 BE. Stream 0: ms since the call's first frame (+20 per frame, wraps). Stream 1: ms since the start of the current speak's audio (0 at each speak's first frame). |
| 9 | 2·n | samples | n signed 16-bit little-endian samples |

v1 format, both directions, fixed: 16 000 Hz, 1 channel, s16le, 20 ms = **320 samples
(649-byte payload)**. A v1 receiver drops a frame with another sample count and sends
`error unsupported_format` (once per call). Stream 0 flows from `call_started` to
`call_ended` (silence included, no gaps but drops); stream 1 only while a speak plays.
The adapter should send stream 1 at real-time pace, at most 1 s ahead; the bridge plays
it at real time and holds at most 2 s (A12). Audio is only sent while a call is active.

### 3.4 Control messages

Every kind-1 payload is one JSON object with a string `type`. Field types: `string`,
`bool`, `u32` (0..=4 294 967 295), `f32`; `?` = optional (may be absent or `null`; senders
omit absent fields). Limits are A14's.

**bridge → adapter**

| type | Fields | Meaning |
|---|---|---|
| `hello` | `protocol` string (`"push-voice-adapter/1"`); `protocols?` [string] (every version offered, includes `protocol`); `bridge_version` string ≤ 64; `language` string (BCP 47); `audio` {`rate` u32 = 16000, `channels` u32 = 1, `format` string = `"s16le"`, `frame_ms` u32 = 20} | First frame on every connection. |
| `call_started` | `call_id`; `peer` string ≤ 64 (display name only); `language` | Start listening; stream-0 audio follows. Also re-sent with the same `call_id` when an adapter (re)connects mid-call (A21). |
| `speak` | `call_id`; `speak_id`; `text` string 1–4 000 chars (plain, prepared for speech); `interruptible` bool; `voice?` string ≤ 64 | Speak this, after any earlier unfinished speak (A9). `interruptible: false` (the bridge's own confirmations) asks the adapter not to cut it on its own barge-in; `stop_speaking` still stops it. |
| `stop_speaking` | `call_id`; `speak_id?` | Stop that speak (playing → cut; queued → dropped), or without `speak_id` every unfinished speak. Each gets its `speak_done {interrupted: true}`. |
| `call_ended` | `call_id`; `reason` (`completed` \| `failed` \| `shutdown` \| `other`) | Stop, answer the outstanding `speak_done`s, release the call's state. No audio for this call after it. |

**adapter → bridge**

| type | Fields | Meaning |
|---|---|---|
| `hello` | `protocol` string (one the bridge offered); `name` string ≤ 64; `version` string ≤ 64; `capabilities` {`stt`, `tts`, `vad`, `barge_in`, `partials`: bool, each default false}; `voices?` [{`id` ≤ 64, `language?`, `name?` ≤ 64}] ≤ 256 | Answer to the bridge's hello. Calls are advertised only with `stt: true`; without `tts` replies stay in the chat. |
| `speech_started` | `call_id`; `utterance_id` | The caller started speaking (only with `vad`). |
| `transcript` | `call_id`; `utterance_id`; `text` 1–4 000 chars; `final` bool; `confidence?` f32 0–1; `language?` | Partial (`final: false`, only with `partials`) or final. One final per utterance; a later final with the same `utterance_id` is ignored. |
| `speak_done` | `call_id`; `speak_id`; `interrupted` bool | That speak finished (its last stream-1 frame was sent before this) or was cut/dropped. |

**either direction**

| type | Fields | Meaning |
|---|---|---|
| `error` | `code` (below); `message` string ≤ 512 (content-free); `call_id?`; `speak_id?`; `utterance_id?` | Something failed. Fatal codes close the connection after sending. |
| `ping` | `id` u32 | Liveness (A11). |
| `pong` | `id` u32 | Answer to the ping with the same `id`. |

Error codes: `unsupported_protocol` (fatal), `unsupported_format` (fatal in a hello
exchange; per-call otherwise), `invalid_frame` (fatal), `invalid_message`,
`engine_unavailable` (the adapter cannot recognise/synthesise right now; the bridge
treats the call as chat-only until the next `call_started`), `overloaded` (fatal from the
bridge, A13; from the adapter: a hint, the bridge slows nothing but logs),
`recognition_failed` (an utterance could not be transcribed: the bridge says "Sorry, I
couldn't make that out."), `synthesis_failed` (with `speak_id`; the sentence is skipped),
`unknown_call` (a `call_id` not active), `busy` (a second `call_started` while a call is
active, or a second bridge on a connect-mode adapter: fatal for that connection),
`internal`, and `other` for codes this version does not know.

### 3.5 Handshake and negotiation

```
bridge                                          adapter
  │── hello {protocol:/1, protocols:[/1], language, audio} ──▶│
  │◀── hello {protocol:/1, name, version, capabilities} ──────│   ≤ 10 s
  │   (calls advertised if capabilities.stt)                  │
```

Nothing but `hello` (and a fatal `error`) may precede a side's hello. The bridge sends
nothing else until the adapter's hello arrived. Mismatch handling: the adapter offered no
common version → `error unsupported_protocol` and close; it cannot take the bridge's
`audio` → `error unsupported_format` and close; the bridge got a version it did not offer
→ `error unsupported_protocol` and close. In both cases the bridge logs one clear line ("voice
adapter speaks push-voice-adapter/2; this bridge speaks push-voice-adapter/1") and backs
off as for a crash. The crate's `negotiate(offered, supported)` picks the highest common
version; `ProtocolVersion` parses `push-voice-adapter/<n>`.

### 3.6 A call

```
owner      bridge (core + adapter client)                         adapter
  │ calls ─▶ answer policy, owner check, answer
  │          ── call_started {call_id, peer, language} ─────────────▶│
  │ audio ─▶ ══ audio stream 0, every 20 ms ════════════════════════▶│ VAD, STT
  │          ◀─ speech_started {utterance_id} ──────────────────────│
  │          (activity "listening"; stop speech, see barge-in)
  │          ◀─ transcript {final:false} … transcript {final:true} ──│
  │          agent.transcript@1 card; prompt or steer to Coda,
  │          or a voice answer to the open card
  │          Coda's reply → Markdown → masked sentences
  │          ── speak {speak_id: s1, text} ─────────────────────────▶│ TTS
  │ ◀ audio  ◀═ audio stream 1 (real time) ═════════════════════════│
  │          ◀─ speak_done {s1, interrupted:false} ─────────────────│
  │  barge-in: ◀─ speech_started ── ; ── stop_speaking ──▶ ; ◀─ speak_done {interrupted:true}
  │ hangs up ─▶ ── call_ended {reason: completed} ──────────────────▶│
```

### 3.7 Rules

- One active call per connection in v1. `call_started` while a call is active is
  answered `error busy` and ignored.
- The bridge never sends credentials, tokens, message history, the owner's or agent's
  ids, or anything but the call's audio, the language, the peer's display name and the
  text to speak.
- The adapter must not log transcripts or speech text by default either (the guide says
  so; the bridge cannot enforce it).

## 4. Lifecycle

States of the bridge's adapter client: `Off` (no `calls.adapter`) → `Starting` (spawning
or connecting) → `Handshaking` (hello sent, 10 s) → `Ready {name, version, capabilities}`
→ on any end → `Backoff {until, last_error}` → `Starting`. `Ready` with `stt: true` makes
calls advertisable (A20).

- **Spawn.** Start the child (A18); hello on stdin. The child exiting, closing stdout,
  failing the handshake or the liveness check ends the session; the bridge kills the
  child (it may still run after closing stdout), waits for it (2 s, then kill), and backs
  off. A child that ran ready for 60 s resets the backoff.
- **Connect.** Connect (A19); `NotFound` (nobody listening) and `PermissionDenied` (another
  user's endpoint) are logged as distinct, clear reasons and back off. The bridge never
  creates the endpoint.
- **Ping/pong.** A11. The 30 s dead-peer timer covers a hung adapter, a full pipe in
  either direction (nothing arrives) and a half-open connection.
- **Bridge restart.** The adapter sees its stream close (spawn: stdin EOF, then the child
  is killed; connect: the connection closes). A connect-mode adapter must accept a new
  bridge connection afterwards and treat every unfinished call as ended.
- **Shutdown.** The bridge sends `call_ended {reason: shutdown}` for a running call, then
  closes stdin / the connection, waits 2 s for a spawned child and kills it.

## 5. Failure matrix

| Failure | Detection | Bridge behaviour | Owner sees |
|---|---|---|---|
| Adapter crashes mid-utterance | EOF / child exit | Call stays up; no transcript for the lost utterance (its audio is not replayed); A21 note; chat-only; restart with backoff; on ready, `call_started` again | "Voice adapter stopped; reconnecting…", then "Voice adapter is back." The owner repeats themselves |
| Adapter crashes mid-speech | EOF / child exit | Playback buffer drained (≤ 2 s) then silence; unfinished speaks dropped; the rest of the reply stays in the chat; A21 | Speech stops; the reply is in the chat |
| Adapter hangs | 30 s without a frame | Treated as a crash; spawn mode kills the child | As above, after ≤ 30 s |
| Slow STT | Nothing to detect | The bridge waits: no timeout on transcripts (engines differ). The activity is "listening" from `speech_started` until the utterance's final transcript, a `recognition_failed`, or 30 s, whichever comes first | Late answers |
| Slow TTS / audio behind | Stream 1 starves | Plays what arrives; gaps are the adapter's | Gaps |
| Adapter floods audio | > 2 s buffered | Drops and counts (A12) | Clipped speech |
| Adapter floods control | > 100/s | `error overloaded`, close, backoff (A13) | As a crash |
| Oversize / bad frame | Header check | `error invalid_frame`, close, backoff | As a crash |
| Wrong sample count | `AudioFormat::check` | Frame dropped, `error unsupported_format` once per call | Missing audio |
| Format refused in hello | `error unsupported_format` instead of a hello | Close, back off, one clear log line; calls not advertised | Agent unreachable for calls |
| Protocol version mismatch | Hello | `unsupported_protocol`, close, back off, one clear log line | Agent unreachable for calls |
| No hello in 10 s | Timer | Close, back off | Unreachable |
| `stt: false` | Hello | Ready but calls not advertised; `status` says why | Unreachable |
| Two adapters on one pipe name | The second's `CreateNamedPipe` with `FILE_FLAG_FIRST_PIPE_INSTANCE` fails `AddrInUse` (the adapter's problem; the guide requires the flag); a squatter of another user fails the bridge's SID check | `PermissionDenied` logged as "the pipe belongs to another user", back off | Unreachable |
| Two bridges on one adapter | The adapter accepts a second connection | Adapter answers `error busy` and closes it (one bridge per adapter in v1) | The second bridge backs off |
| Bridge restarts mid-call | — | The call ends (the media helper dies with the bridge, as today); the adapter sees EOF | Call drops |
| `engine_unavailable` | Error | Chat-only for this call (A21 note without reconnecting) | "Voice adapter cannot listen right now." |
| Adapter not found (spawn) | Spawn error | Clear log line with the program path's file name, back off | Unreachable |

## 6. Threat model

Assets: the owner's audio and words (content), the agent's replies, the bridge's
credentials (agent key, backend API key), the owner's control over the agent.

| Threat | Mitigation |
|---|---|
| A local process of another user impersonates the adapter (connect mode) | push-bridge-control transport: pipe server SID must be the user or SYSTEM, remote pipes refused; Unix socket owner = runtime dir owner, 0700 dir, 0600 socket. Spawn mode: the bridge created the child; the pipes are anonymous. |
| A process impersonates the bridge to a connect-mode adapter (to inject audio/speech or harvest transcripts) | The guide requires the adapter to create its pipe with DACL user+SYSTEM, `FILE_FLAG_FIRST_PIPE_INSTANCE`, `PIPE_REJECT_REMOTE_CLIENTS`, and to check the client's user SID (`GetNamedPipeClientProcessId` → token); Unix: 0600 socket in a 0700 dir and `SO_PEERCRED`/`getpeereid` uid check. A same-user process is inside the trust boundary (it could read the bridge's files anyway). |
| Transcript injection ("/mode bypass" spoken) | A transcript is typed owner text, nothing more: masked like typed text, never a `/` command (the bridge refuses to forward a transcript starting with `/` as a command, as PR 7 does), the owner-only rules apply, risky actions still need their confirmation card. The adapter is the owner's own process, so it is as trusted as the owner's keyboard — no more. |
| Resource exhaustion | Header-checked 64 KiB cap; text caps (A14); 1 s / 2 s audio queues with drops; 100 control msgs/s; ≤ 16 unfinished speaks; one call; 30 s dead peer; hello in 10 s; decode errors bounded and content-free. |
| Content leaks to logs | A15: `Debug` without text; decode errors without input; ids are validated printable ASCII (no log injection) and are the only per-message values logged; unknown type names are sanitized and truncated; metrics carry counts only; the control channel's call info is content-free (unchanged). |
| Credentials reaching the adapter | Nothing in the protocol carries them; spawn strips secret env vars (A18); the adapter gets no file paths from the bridge. |
| Spawn hijack | No shell, argv list, absolute path recommended (validation warns when relative), no window, same user, stderr to a file only the user can read. |
| Fuzzed/garbage input | The decoder is fuzzed (A17) and property-tested: no panic, no unbounded allocation, deterministic errors. |

## 7. Crate layout (PR 1)

`crates/push-voice-adapter-proto` (lib `push_voice_adapter_proto`), deps: serde,
serde_json, thiserror, schemars 1 (derive), tokio (optional, `io-util`; default feature
`tokio`).

| Module | Contents |
|---|---|
| `frame` | `Kind`, `Stream`, `Frame {Control(Vec<u8>), Audio(AudioFrame)}`, `AudioFrame {stream, seq, timestamp_ms, samples}`, `encode`/`Frame::to_bytes`, `FrameDecoder` (sans-IO, incremental), `FrameError`, constants `MAX_PAYLOAD`, `HEADER_LEN`, `AUDIO_HEADER_LEN`. |
| `io` | `FrameReader<R: Read>`, `FrameWriter<W: Write>`, `ReadError`. |
| `tokio_io` (feature `tokio`) | `AsyncFrameReader<R: AsyncRead + Unpin>` (cancel-safe `read_frame`), `AsyncFrameWriter<W: AsyncWrite + Unpin>`. |
| `message` | `BridgeMessage`, `AdapterMessage` (serde `type`-tagged), every message struct, `Capabilities`, `AudioSpec`, `Voice`, `ErrorCode`, `CallEndReason`, `Decoded<T> {Known(T), Unknown(String)}`, `decode_bridge`, `decode_adapter`, `encode`, `MessageError`, `Invalid`, validation. |
| `version` | `PROTOCOL` (`"push-voice-adapter/1"`), `ProtocolVersion`, `negotiate`. |
| `format` | `AudioFormat` (`V1`: 16 000 Hz, 1 channel, s16le, 20 ms, 320 samples), `check`. |
| `limits` | every cap and timer of A11–A14 as constants. |
| `schema` | `schemas() -> Vec<(file name, JSON)>` for the checked-in files. |

## 8. Bridge architecture (PR 2)

What exists (PR 7/8): `voice::session::Session` runs a call's pipeline (VAD → STT worker,
TTS worker → `Speaker`), reporting `SessionEvent`s to the core (`core/voice.rs`,
`core/calling.rs`), which owns answering, prompts/steers, card matching
(`voice/confirm.rs`), speech text (`voice/speech.rs`, masking), transcripts
(`agent.transcript@1`), activity and the tray's call state.

PR 2 keeps the core untouched in spirit and swaps what is under `Session`:

- **`voice/engine.rs`** (new): `trait SpeechEngine` — `hear(frame)`, `say(text, own)`,
  `stop()`, `end(reason)`, reporting through the session's `SessionEvent` sink. `Session`
  keeps the line (media, pacing, `Speaker`, hang-up, barge-in generation, mute/silence,
  `SessionEvent`s) and delegates speech to an engine. Two engines: `BuiltinEngine` (today's
  VAD/STT/TTS, moved, removed in PR 3) and `AdapterEngine`.
- **`calls.adapter`** in `config.rs` (spawn/connect parsing and validation; `adapter` and
  `stt` other than `none` are mutually exclusive in PR 2).
- **`voice/adapter/`** (new): `spawn.rs` (A18: argv, env stripping, no window, stderr
  log, kill-on-drop/job object), `connect.rs` (A19 via
  `push_bridge_control::transport::connect` with an `Endpoint` built from the configured
  name/path), `link.rs` (one live connection: hello, ping/pong, dead peer, rate caps, the
  writer's control-first queue), `supervisor.rs` (the state machine of §4 and backoff;
  publishes `AdapterState` through a `watch` channel), `router.rs` (`AdapterEngine`: one
  call at a time: stream-0 audio out from the
  line with the 1 s drop queue, stream-1 audio into the line's `Speaker` with the 2 s cap,
  `speech_started` → `SessionEvent::Listening(true)` + barge-in, final `transcript` →
  `SessionEvent::Transcript {text, started}` where `started` is when that utterance's
  `speech_started` arrived (or the transcript's own arrival without `vad`), `speak_done` →
  `Speaking(false)` when nothing is left, `recognition_failed` → `SpeechFailed`).
- **Kept in the core, unchanged in behaviour**: answer policy (auto/ask), owner-only, DND,
  rate limit, notify_by_call, prompt/steer, voice answers and card editing, `SpeechText`
  (Markdown → sentences, masked; each sentence is one `speak`, `interruptible: true`; the
  bridge's own lines `interruptible: false`), transcripts in chat, activity labels, tray
  call state, Mute agent (`stop_speaking` + nothing sent until unmuted), Hang up
  (`call_ended`).
- **Advertising** (A20): `main.rs` derives `sdk.calls` from the adapter state; push-sdk
  gains `Client::set_calls_advertised`. The core declines nothing new: without a ready
  adapter the server answers `UNREACHABLE`.
- **Fallback** (A21): the router reports `AdapterLost`/`AdapterBack`; the core posts the
  notes, keeps the call, stops speaking, and keeps posting the agent's replies in the chat.
- **Status and tray**: `status` gains `Voice adapter: ready (<name> <version>; stt, tts,
  vad, barge-in)` / `starting` / `backing off (reason)`; the control channel's `Status`
  gains an additive content-free `adapter {state, name?, version?, capabilities?}`; the
  tray's Overview shows it.
- **Metrics** (log counters at shutdown and in `status`; the bridge has no metrics
  endpoint): frames in/out, dropped frames by direction, adapter restarts, speak latency
  (final transcript → first `speak`, and `speak` → first stream-1 frame) as last/avg/max.
- **Testkit** `crates/push-voice-adapter-testkit`: a library (`ScriptedAdapter`) and a
  binary `push-voice-adapter-testkit` (stdio, or `--pipe`/`--socket` with the same
  endpoint security via push-bridge-control's `Listener`) driven by a JSON script: tones
  in stream 0 (Goertzel, the PR 7 fixtures' frequency rules) map to words → transcripts;
  `speak` answers 600 Hz tone frames for a length that follows the text; switches to
  crash after N frames, refuse a format, speak another version, stall, flood.
- **E2E** (`crates/push-smoke/tests/bridge_voice.rs`, job `bridge-voice`): the existing
  tests run through the testkit adapter instead of the fakes, plus: spawn and connect
  modes, a crash mid-call → fallback → reconnect resumes, no adapter → not advertised, a
  format mismatch → clear error; timing assertions (final transcript → `speak` < 100 ms
  excluding the backend; stream-1 playback without gaps over 40 ms); content-free logs.

## 9. Migration (PR 3)

- Remove `voice/{azure,local,provider,vad,wav,fake,test_server}.rs` (keeping only what
  tests still need, moved under the testkit), `BuiltinEngine`, the reqwest dependency if
  nothing else uses it, `calls.stt`, `calls.tts`, `calls.vad`.
- Config: `calls.stt`, `calls.tts` or `calls.vad` present → startup error naming the key:
  "calls.stt is no longer supported: speech runs in a voice adapter. Configure
  calls.adapter (docs/agent-bridge.md §18)". Nothing silently changes behaviour.
- The `voice` feature stays (media helper only). `status`'s Voice line shows the adapter.
- Docs: docs/agent-bridge.md §18 rewritten for adapters; docs/calls.md §19; complete
  docs/voice-adapter-protocol.md with "Writing an adapter" (pseudo-code) and
  troubleshooting (no audio, adapter not found, pipe in use, version mismatch); voice
  spec §10.3 marked superseded by this addendum; CHANGELOG `### Removed` + `### Changed`
  with the migration.
- push-sdk: document `push_sdk::media`'s 16 kHz mono conversion as the adapter-facing
  format (no API change).

## 10. Testing

PR 1: unit tests (every message's round trip, unknown fields and types, validation of
every cap, `Debug` without content, version parsing and negotiation, the v1 format
check), property tests (proptest: any frame sequence encoded then fed in any split is
decoded identically; random bytes never panic and either decode or fail with a
`FrameError`; message round trips), the schema test (A16), the fuzz target (A17). PR 2
and PR 3 tests: §8 and the plan.

## 11. Compatibility

The protocol only grows: new message types, new optional fields, new error codes and
capabilities; never a changed meaning. Breaking changes need `push-voice-adapter/2`,
negotiated (A6). The bridge's `calls` config gains `adapter` in PR 2 (additive) and loses
`stt`/`tts`/`vad` in PR 3 (a startup error, never a silent change).

## 12. Implementation notes

### PR 1 (protocol crate): decisions and deviations

- **Public API** (`push_voice_adapter_proto`): `Frame {Control(Vec<u8>), Audio(AudioFrame)}`,
  `AudioFrame {stream, seq, timestamp_ms, samples}`, `Kind`, `Stream`, `FrameError`,
  `Frame::{encode_into, to_bytes}`, `FrameDecoder::{push, next_frame, buffered}`;
  `FrameReader`/`FrameWriter` (blocking) and `AsyncFrameReader`/`AsyncFrameWriter`
  (feature `tokio`, default; `read_frame` cancel-safe), `ReadError`, `WriteError`;
  `BridgeMessage`, `AdapterMessage` and their structs, `Message` (`TYPES`,
  `message_type`, `validate`), `decode::<M>() -> Decoded<M> {Known, Unknown(type)}`,
  `encode()`, `MessageError`, `Invalid {field, problem}`; `PROTOCOL`, `SUPPORTED`,
  `ProtocolVersion`, `negotiate`; `AudioFormat::V1` (`samples_per_frame`, `spec`,
  `from_spec`, `check`); `limits::*`; `schema::{schemas, render, DIR}`.
- **Codec is format-agnostic** (any even, non-zero sample count up to the frame cap); the
  v1 sample count is the receiver's check (`AudioFormat::check`) so a later version can
  negotiate other frame sizes without a codec change.
- **Decoding** parses JSON once into a `Value`, looks up `type` among the direction's
  known types (unknown → `Decoded::Unknown` with the name sanitized to `[a-z0-9_.-]`, 64
  chars), then deserializes and validates. serde errors are reduced to the message type
  and, for "missing field", the field name (our own names only), because serde's messages
  can echo input values.
- **`other`**: unknown `error.code` / `call_ended.reason` values decode as `Other`
  (`#[serde(other)]`), which is skipped in the schemas (never sent).
- **Validation beyond the request**: ids printable ASCII 1–64 bytes (log-safe), names
  without control characters, empty transcript/speak text refused, `peer` may be empty.
  `Debug` also redacts `peer` and `error.message` (lengths only), not just text.
- **Schemas**: schemars 1.2 (`derive`, MIT; `schemars_derive` and
  `serde_derive_internals` new in Cargo.lock; cargo-deny clean; builds on 1.85). The
  per-message files are each struct's schema with the `type` constant injected; the two
  unions are the enums' schemas. Not a cargo feature: the schema test must run in the
  ordinary `cargo test --workspace`.
- **Fuzzing**: `crates/push-voice-adapter-proto/fuzz` is its own workspace (excluded from
  the main one; its own Cargo.lock) with targets `frames` and `messages`; the CI job
  `voice-adapter-fuzz` installs cargo-fuzz 0.13.2 on nightly and runs 60 s + 30 s. The
  proptest suite (`tests/properties.rs`) is the in-CI equivalent on stable.
- **Request deviations, all additive**: `hello.protocols` (A6); `error.speak_id` /
  `utterance_id` (A9); error codes `invalid_frame`, `invalid_message`,
  `recognition_failed`, `synthesis_failed`, `unknown_call`, `busy`, `internal` added to the
  request's examples; `call_ended.reason` a tolerant set (`completed`, `failed`,
  `shutdown`). An adapter-side `audio` in the adapter's hello was **not** added: the
  bridge's `audio` is the v1 format, and an adapter that cannot take it answers
  `unsupported_format`.
