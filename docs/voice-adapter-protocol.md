# Voice adapter protocol (`push-voice-adapter/1`)

push-agent-bridge carries a call's audio; it does not recognise or produce speech. A
separate **voice adapter** process does: it hears the caller's audio and sends back what
was said, and it turns the agent's replies into audio. Any engine fits behind it (local
models, a cloud speech service, a realtime speech model), in any language, released on
its own schedule. This page is the complete wire reference: everything needed to write an
adapter without reading the bridge's source.

- Design and rationale: [voice adapter design](superpowers/specs/2026-11-04-voice-adapter-design.md)
  (an addendum to the [voice calls design](superpowers/specs/2026-11-03-voice-calls-design.md)).
- JSON Schemas of every control message: [`docs/schemas/voice-adapter/v1/`](schemas/voice-adapter/v1/).
- Rust implementation (framing, messages, limits, version negotiation):
  `crates/push-voice-adapter-proto`.

**Status.** The protocol and its crate are released first. The bridge's adapter client
(`calls.adapter` in the bridge's config, spawn and connect modes) and the test adapter
`crates/push-voice-adapter-testkit` come next; then the bridge's built-in speech providers
(`calls.stt`, `calls.tts`) are removed. Until the client ships, docs/agent-bridge.md §18
describes the built-in providers.

Contents: [1 Overview](#1-overview) · [2 Transport](#2-transport) ·
[3 Framing](#3-framing) · [4 Audio](#4-audio) · [5 Control messages](#5-control-messages) ·
[6 Handshake and versions](#6-handshake-and-versions) · [7 A call](#7-a-call) ·
[8 Speaking](#8-speaking) · [9 Liveness](#9-liveness) ·
[10 Limits and backpressure](#10-limits-and-backpressure) · [11 Errors](#11-errors) ·
[12 Compatibility](#12-compatibility) · [13 Privacy and security](#13-privacy-and-security)

## 1. Overview

```
 owner's device ──WebRTC/Opus── push-agent-bridge ──push-voice-adapter/1── voice adapter
                                 (answers, routes,     stdin/stdout, or     (VAD, STT, TTS,
                                  carries audio)       a local pipe/socket   its own keys)
```

One bridge talks to one adapter over one byte stream. Both sides send **frames**: JSON
control messages and raw PCM audio. The bridge sends the caller's audio continuously
during a call; the adapter says when the caller starts speaking and what they said; the
bridge asks the adapter to speak text and plays the audio it sends back.

## 2. Transport

The bridge's config chooses one of two modes (one adapter per bridge instance):

**Spawn.** `{"spawn": {"command": ["C:\\Tools\\voice-adapter.exe", "--config", "…"]}}`.
The bridge starts the adapter as a child process (no window, no shell, arguments as a
list) and talks over its **stdin** (bridge → adapter) and **stdout** (adapter → bridge).
stdout carries frames only: an adapter must never print anything else there; logging goes
to stderr (the bridge writes it to its log folder or discards it). The bridge restarts the
adapter with a backoff (1 s, doubling to 60 s) when it exits or fails, and kills it when
the bridge stops. The environment is the bridge's minus its secrets.

**Connect.** `{"connect": {"pipe": "push-voice-<name>"}}` (Windows, `\\.\pipe\push-voice-<name>`)
or `{"connect": {"socket": "$XDG_RUNTIME_DIR/push-voice/<name>.sock"}}` (Unix). The
adapter is already running and **listens**; the bridge connects (and reconnects with the
same backoff). The endpoint must be private to the user, exactly like the bridge's control
channel:

| | Windows named pipe | Unix socket |
|---|---|---|
| Create | `CreateNamedPipeW` with `FILE_FLAG_FIRST_PIPE_INSTANCE` on the first instance (a second adapter, or a squatter, then fails instead of sharing the name) and `PIPE_REJECT_REMOTE_CLIENTS` | socket in a directory of mode 0700 owned by the user; the socket 0600 |
| Access | DACL `D:P(A;;GA;;;<user SID>)(A;;GA;;;SY)`: the user and SYSTEM only | the modes above |
| Check the bridge | `GetNamedPipeClientProcessId` → the process token's user SID must be the adapter's user (or SYSTEM) | `SO_PEERCRED` / `getpeereid`: the peer uid must be the adapter's |
| The bridge checks | the pipe server's user SID is the bridge's user (or SYSTEM) | the socket's owner is the runtime dir's owner |

A connect-mode adapter serves **one bridge at a time** in v1: it answers a second
connection with `error busy` and closes it. When the bridge's connection closes, every
call on it is over; the adapter goes back to waiting for a connection.

## 3. Framing

Every frame is:

```
 ┌──────────────────┬──────────┬─────────────────────────────┐
 │ len: u32, big-   │ kind: u8 │ payload: len bytes          │
 │ endian           │          │                             │
 └──────────────────┴──────────┴─────────────────────────────┘
```

- `len` counts the **payload only** (not itself, not `kind`), at most **65 536**.
- `kind` 1: a JSON control message — one UTF-8 JSON object (no BOM), 1 to 65 536 bytes.
- `kind` 2: audio — a 9-byte header and the samples ([§4](#4-audio)), 11 to 65 536 bytes,
  an odd number of sample bytes is malformed.
- Frames follow each other directly. A reader must handle a frame split over several
  reads and several frames in one read. A stream that ends between frames is a clean
  close; one that ends inside a frame is an error.
- Any other `kind`, a `len` over the limit (refuse it from the 5-byte header, before
  reading the payload), an empty control frame or a malformed audio payload is a
  **framing error**: send `error invalid_frame` if you can, then close.

Example: `{"type":"ping","id":7}` (22 bytes) as a frame:

```
00 00 00 16  01  7b 22 74 79 70 65 22 3a 22 70 69 6e 67 22 2c 22 69 64 22 3a 37 7d
└─ len 22 ─┘ └kind┘ └──────────────── {"type":"ping","id":7} ────────────────┘
```

## 4. Audio

An audio payload:

| Offset | Size | Field | Meaning |
|---|---|---|---|
| 0 | 1 | `stream` | **0** = the caller's audio (bridge → adapter); **1** = speech for the caller (adapter → bridge). Any other value is a framing error. |
| 1 | 4 | `seq` | u32 big-endian. Per stream and call: 0, 1, 2, … (wrapping). A gap means frames were dropped. |
| 5 | 4 | `timestamp_ms` | u32 big-endian. Stream 0: ms since the call's first frame (+20 per frame). Stream 1: ms since the start of the current speak's audio (0 at its first frame). |
| 9 | 2·n | samples | n signed 16-bit **little-endian** samples |

**Format, fixed in v1, both directions:** PCM s16le, **16 000 Hz, mono, 20 ms frames of
320 samples** (a 649-byte payload, a 654-byte frame). Later protocol versions may add
formats; they are negotiated, never assumed. A receiver drops a v1 frame of another size
and sends `error unsupported_format` (once per call). The bridge's media helper converts
the call's Opus 48 kHz to and from this format, so an adapter never sees Opus or WebRTC.

Example: the first 20 ms of a call (320 zero samples):

```
00 00 02 89  02  00  00 00 00 00  00 00 00 00  00 00 00 00 … (640 bytes)
└ len 649 ─┘ kind stream └─seq 0─┘ └─ts 0 ms─┘ └── samples ──┘
```

- **Stream 0** flows from `call_started` to `call_ended`, every 20 ms, silence included.
  The bridge never pauses it, but drops frames if the adapter does not keep up
  ([§10](#10-limits-and-backpressure)).
- **Stream 1** carries the audio of the speak being played ([§8](#8-speaking)). Send it at
  real-time pace (at most about 1 s ahead); the bridge plays it in real time and buffers at
  most 2 s, dropping what does not fit. When the bridge has nothing queued (a speak's audio
  starts, or the audio fell behind) it holds the first frames until 60 ms are there or the
  first waited 60 ms, so a timer's jitter of a few tens of milliseconds is not heard.
  Sending the first frames of a speak a little ahead (100–200 ms) is welcome: it absorbs
  larger stalls and costs nothing.
- Audio is only sent while a call is active.

## 5. Control messages

Each kind-1 payload is one JSON object with a string **`type`**. Optional fields (`?`)
may be absent or `null`; senders omit them when absent. **Ignore fields you do not know,
and ignore messages whose `type` you do not know.** Strings are UTF-8; "characters" means
Unicode scalar values. Ids (`call_id`, `speak_id`, `utterance_id`) are opaque: 1–64
printable ASCII characters (`!` to `~`, no spaces). Languages are BCP 47 tags such as
`en-US` (1–35 of `A-Z a-z 0-9 -`).

### Bridge → adapter

#### `hello`

The bridge's first message on every connection. Schema: [bridge-hello.json](schemas/voice-adapter/v1/bridge-hello.json).

| Field | Type | |
|---|---|---|
| `protocol` | string | The bridge's preferred version, `"push-voice-adapter/1"` |
| `protocols` | string[]? | Every version it speaks (≤ 16); absent = only `protocol` |
| `bridge_version` | string ≤ 64 | For your diagnostics |
| `language` | string | The language spoken on calls (`calls.language`) |
| `audio` | object | `{"rate": 16000, "channels": 1, "format": "s16le", "frame_ms": 20}` |

```json
{"type":"hello","protocol":"push-voice-adapter/1","protocols":["push-voice-adapter/1"],
 "bridge_version":"0.1.0","language":"en-US",
 "audio":{"rate":16000,"channels":1,"format":"s16le","frame_ms":20}}
```

#### `call_started`

A call starts: listen to stream 0. Also sent again, with the same `call_id`, when an
adapter (re)connects while a call runs. Schema: [call-started.json](schemas/voice-adapter/v1/call-started.json).

| Field | Type | |
|---|---|---|
| `call_id` | id | Chosen by the bridge per call; not the messaging server's call id |
| `peer` | string ≤ 64 | The caller's display name only (may be empty) |
| `language` | string | BCP 47 |

#### `speak`

Speak this text, after any earlier speak that is not done ([§8](#8-speaking)). Schema:
[speak.json](schemas/voice-adapter/v1/speak.json).

| Field | Type | |
|---|---|---|
| `call_id` | id | |
| `speak_id` | id | Chosen by the bridge |
| `text` | string, 1–4 000 chars | Plain text already prepared for speech (no Markdown; code and tables replaced by a short sentence; secrets masked) — usually one sentence |
| `interruptible` | bool | `false` for the bridge's own short lines (confirmations): do not cut them on your own barge-in. `stop_speaking` always stops them. |
| `voice` | string? ≤ 64 | One of your hello's `voices`; absent: your default |

#### `stop_speaking`

Stop speaking: barge-in from the bridge's side, the owner muting the agent, or a hang-up.
Schema: [stop-speaking.json](schemas/voice-adapter/v1/stop-speaking.json).

| Field | Type | |
|---|---|---|
| `call_id` | id | |
| `speak_id` | id? | That speak only (playing: cut it; queued: drop it). Absent: **every** speak not done. |

Each stopped or dropped speak gets its `speak_done` with `interrupted: true`. With nothing
to stop it is a no-op.

#### `call_ended`

The call is over: stop listening and speaking, send the outstanding `speak_done`s if you
like (the bridge no longer needs them), release the call's state. Schema:
[call-ended.json](schemas/voice-adapter/v1/call-ended.json).

| Field | Type | |
|---|---|---|
| `call_id` | id | |
| `reason` | string | `completed` (someone hung up), `failed` (media or signalling failed), `shutdown` (the bridge stops); treat unknown values like `completed` |

### Adapter → bridge

#### `hello`

Your answer to the bridge's hello, within **10 s**. Schema: [adapter-hello.json](schemas/voice-adapter/v1/adapter-hello.json).

| Field | Type | |
|---|---|---|
| `protocol` | string | The version you chose: one the bridge offered ([§6](#6-handshake-and-versions)) |
| `name` | string, 1–64 | Shown in the bridge's status and the tray |
| `version` | string ≤ 64 | Shown with the name |
| `capabilities` | object | `stt`, `tts`, `vad`, `barge_in`, `partials`: booleans, absent = `false` |
| `voices` | object[]? ≤ 256 | `{"id": ≤ 64, "language"?: BCP 47, "name"?: ≤ 64}` |

| Capability | Meaning | Without it |
|---|---|---|
| `stt` | You send `transcript`s | The bridge does not advertise calls at all: the owner sees the agent as unreachable |
| `tts` | `speak` gets stream-1 audio | Replies stay in the chat; the bridge sends no `speak` |
| `vad` | You send `speech_started` | No barge-in at speech onset; the bridge stops speech when a final transcript arrives |
| `barge_in` | You stop your own speech when the caller speaks (only `interruptible` speaks) | The bridge's `stop_speaking` does it, a round trip later |
| `partials` | You send `transcript` with `final: false` | Nothing |

```json
{"type":"hello","protocol":"push-voice-adapter/1","name":"my-adapter","version":"0.3.0",
 "capabilities":{"stt":true,"tts":true,"vad":true,"barge_in":false,"partials":false},
 "voices":[{"id":"en-US-JennyNeural","language":"en-US"}]}
```

#### `speech_started`

The caller started speaking (adapters with `vad`). The bridge stops any speech
(barge-in) and shows the agent as "listening". Schema: [speech-started.json](schemas/voice-adapter/v1/speech-started.json).

| Field | Type | |
|---|---|---|
| `call_id` | id | |
| `utterance_id` | id | Chosen by you; the same id goes on this utterance's transcripts |

#### `transcript`

What the caller said. Schema: [transcript.json](schemas/voice-adapter/v1/transcript.json).

| Field | Type | |
|---|---|---|
| `call_id` | id | |
| `utterance_id` | id | |
| `text` | string, 1–4 000 chars | Never empty: if nothing was recognised, send nothing (or `error recognition_failed`) |
| `final` | bool | `false`: a partial (only with `partials`; the bridge uses it for the activity only). `true`: the utterance is complete and the bridge acts on it. One final per utterance; later ones with the same id are ignored. |
| `confidence` | number? 0–1 | |
| `language` | string? | The language recognised |

A final transcript is exactly like text the owner typed: it becomes the agent's next
prompt (or a steer while the agent works), or the answer to an open card ("approve",
"option two"), and it is posted in the chat. It is never a command channel: a transcript
starting with `/` is not run as a command.

#### `speak_done`

A speak finished — after you sent its last stream-1 frame — or was cut or dropped.
Schema: [speak-done.json](schemas/voice-adapter/v1/speak-done.json).

| Field | Type | |
|---|---|---|
| `call_id` | id | |
| `speak_id` | id | |
| `interrupted` | bool | `true` when stopped, dropped or failed before its end |

### Either direction

#### `error`

Schema: [error.json](schemas/voice-adapter/v1/error.json).

| Field | Type | |
|---|---|---|
| `code` | string | [§11](#11-errors); treat unknown codes as `internal` |
| `message` | string ≤ 512 | Short and **content-free**: never audio, transcript or speech text; no control characters |
| `call_id` | id? | |
| `speak_id` | id? | |
| `utterance_id` | id? | |

#### `ping` / `pong`

`{"type":"ping","id":<u32>}`, answered by `{"type":"pong","id":<same>}` ([§9](#9-liveness)).
Schemas: [ping.json](schemas/voice-adapter/v1/ping.json), [pong.json](schemas/voice-adapter/v1/pong.json).

The unions of each direction: [bridge-message.json](schemas/voice-adapter/v1/bridge-message.json),
[adapter-message.json](schemas/voice-adapter/v1/adapter-message.json).

## 6. Handshake and versions

1. The bridge connects (or starts you) and sends its `hello` first. Nothing else comes
   from the bridge until your `hello` arrives.
2. You choose the **highest** version you speak among the bridge's `protocols` (or its
   `protocol` when `protocols` is absent) and answer your `hello` with it, within 10 s.
   Before your hello you may send only a fatal `error`.
   - No common version: send `error unsupported_protocol` (say which versions you speak
     in `message`) and close.
   - You cannot take the bridge's `audio` format: send `error unsupported_format` and
     close.
3. The bridge checks your `protocol` is one it offered (else `error unsupported_protocol`
   and close) and logs one clear line on any mismatch, then retries with its backoff.

Versions are `push-voice-adapter/<n>` with a decimal `n ≥ 1` (no leading zeros). This page
describes `n = 1`.

## 7. A call

```mermaid
sequenceDiagram
    participant O as Owner (device)
    participant B as push-agent-bridge
    participant A as Voice adapter
    B->>A: hello {protocol, protocols, language, audio}
    A->>B: hello {protocol, name, version, capabilities}
    Note over B: calls advertised (capabilities.stt)
    O->>B: call (WebRTC)
    B->>A: call_started {call_id, peer, language}
    loop every 20 ms
        B->>A: audio stream 0 (caller)
    end
    A->>B: speech_started {utterance_id}
    A->>B: transcript {final: false} (optional)
    A->>B: transcript {final: true, text}
    Note over B: posted in the chat; prompt or steer to the agent, or a card answer
    B->>A: speak {speak_id: s1, text}
    B->>A: speak {speak_id: s2, text}
    A->>B: audio stream 1 (s1, real time)
    A->>B: speak_done {s1, interrupted: false}
    A->>B: audio stream 1 (s2)
    A->>B: speech_started {utterance_id: u2}
    Note over B: barge-in: playback cleared, rest of the reply muted
    B->>A: stop_speaking {call_id}
    A->>B: speak_done {s2, interrupted: true}
    O->>B: hang up
    B->>A: call_ended {reason: completed}
```

Rules:

- **One active call per connection** in v1. A `call_started` while a call is active is
  answered `error busy` and ignored. `call_id` is on every message so a later version can
  carry several.
- Send nothing about a call before its `call_started` or after its `call_ended`; a
  message about another `call_id` is answered `error unknown_call` and ignored.
- If the adapter goes away during a call, the bridge keeps the call up, tells the owner
  ("Voice adapter stopped; reconnecting…"), continues by chat only, and sends
  `call_started` again with the same `call_id` once an adapter is back. The utterance or
  speech in progress is lost.

## 8. Speaking

- Speak `speak` requests **strictly in the order received, one at a time**. Stream-1
  audio always belongs to the oldest speak that is not done; send its `speak_done` after
  its last audio frame, then start the next.
- **Every `speak` gets exactly one `speak_done`**: `interrupted: false` when it played to
  the end; `true` when `stop_speaking` cut or dropped it, when your own barge-in cut it, or
  when synthesis failed (send `error synthesis_failed` with its `speak_id` first).
- The bridge keeps at most 16 speaks not done and queues the rest itself, so start
  synthesising the next one while the current one plays to avoid gaps.
- Barge-in: when the caller speaks while you speak, send `speech_started`. The bridge
  clears its playback buffer and sends `stop_speaking` (all speaks). With `barge_in` you may
  stop interruptible speaks yourself right away; `stop_speaking` then finds nothing to stop.
- Audio frames for a speak that was stopped are dropped by the bridge; stop sending them.

## 9. Liveness

Both sides send `ping` every **10 s** with a new `id` and answer every `ping` at once with
a `pong` of the same `id`. Any frame received counts as a sign of life; **30 s** without
one means the peer is dead: close. (Spawn mode: the bridge then kills and restarts the
adapter.) Keep reading your input while busy: a blocked reader stalls the bridge's writes
too.

## 10. Limits and backpressure

| Limit | Value | When exceeded |
|---|---|---|
| Frame payload | 65 536 bytes | Framing error: `invalid_frame`, close |
| `transcript.text`, `speak.text` | 1–4 000 characters | `invalid_message`, the message is ignored |
| `error.message` | 512 characters | `invalid_message` |
| Names, versions, voices, `peer` | 64 characters | `invalid_message` (a hello: close) |
| Ids | 1–64 printable ASCII | `invalid_message` |
| Voices in a hello | 256 | `invalid_message`, close |
| Control messages from the adapter | 100 per second | `error overloaded`, close |
| Partial transcripts acted on | 10 per second | dropped |
| Caller audio queued by the bridge | 1 s (50 frames) | the oldest frame is dropped (a `seq` gap) |
| Speech audio buffered by the bridge | 2 s (100 frames) | frames dropped |
| Speaks not done | 16 | the bridge holds the rest |
| Hello answer | 10 s | close |
| Silence from the peer | 30 s | close |

Control messages are never dropped and go before queued audio. A malformed control
message (bad JSON, a missing field, a broken limit) is answered `error invalid_message`
and ignored — except a malformed `hello`, which ends the connection.

## 11. Errors

| Code | Sent by | Meaning | After it |
|---|---|---|---|
| `unsupported_protocol` | either | No common version | sender closes |
| `unsupported_format` | either | Instead of a hello: the audio format cannot be used. During a call: an audio frame of the wrong size | instead of a hello: sender closes; else continue |
| `invalid_frame` | either | A framing error | sender closes |
| `invalid_message` | either | A control message was malformed and ignored | continue |
| `engine_unavailable` | adapter | You cannot recognise or synthesise now (an engine down, a quota) | the bridge continues the call by chat only until the next `call_started` |
| `overloaded` | either | Too much: from the bridge, too many messages (it closes); from you, a hint | — |
| `recognition_failed` | adapter | An utterance (`utterance_id`) could not be transcribed | the bridge says "Sorry, I couldn't make that out." |
| `synthesis_failed` | adapter | A speak (`speak_id`) could not be synthesised; its `speak_done` follows | the sentence is skipped |
| `unknown_call` | either | A `call_id` that is not active | continue |
| `busy` | adapter | A second call, or a second bridge on a connect-mode adapter | for a second bridge: close that connection |
| `internal` | either | Anything else | continue or close |

## 12. Compatibility

- The protocol only grows: new message types, new optional fields, new error codes, new
  capabilities. **Ignore what you do not know**: unknown fields, unknown `type`s, unknown
  `reason`s and codes. Never send a field or type this page does not list.
- A change in meaning, a new frame kind or a new audio format needs a new version,
  negotiated in the hellos ([§6](#6-handshake-and-versions)).
- The schemas describe what a v1 sender sends; receivers must be more lenient than them.

## 13. Privacy and security

- The bridge sends you only the call's audio, the language, the caller's display name and
  the text to speak. Never credentials, tokens, ids that link to the messaging server, or
  message history. Your own engine credentials stay in your process.
- **Transcripts, speech text and audio are content.** The bridge never writes them to its
  logs, metrics or control channel; do the same by default (log ids, codes, lengths and
  durations). Error messages must be content-free.
- A transcript is treated as text the owner typed — same masking, same owner-only rules,
  confirmation cards for risky actions — never as a command.
- Spawn mode: you run as the same user as the bridge, started without a shell, with the
  arguments as a list, and an environment without the bridge's secrets
  (`CODA_SERVE_API_KEY` and others). Connect mode: make your endpoint private as in
  [§2](#2-transport) and check the connecting process's user.
- Cap what you accept from the bridge as the bridge caps what it accepts from you
  ([§10](#10-limits-and-backpressure)).
