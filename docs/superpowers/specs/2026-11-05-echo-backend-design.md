# Echo backend for push-agent-bridge: design

Change request: a deterministic, credential-free test agent that runs under
`push-agent-bridge` (TPM agent-key sign-in, enrollment, tray, cards, calls) and behaves like
the Node echo sample (not available here; this follows the request's summary).

## Config

```json
{ "backend": "echo", "echo": { "answerTimeoutS": 300 } }
```

`echo` is optional (default `answerTimeoutS` 300); `Config::validate` refuses a timeout
outside 10–3600 s and an `echo` section with `"backend": "coda"` is ignored (like `coda` with
`echo`). Everything else (`api_url`, `key`, `roots`, `default_folder`, `control`, `peek`,
`calls`) is as for Coda; `roots`/`default_folder` stay required (the core needs a folder),
any existing folder works.

## Approaches considered

1. **Backend only, no core change.** Impossible for three of the behaviours: the card
   deadline (10 min) and the activity labels are fixed in the core, and the backend never
   sees a message's timestamp. A backend-side timer would leave the core's card open, so
   the next text would be taken as its answer.
2. **Small `Backend` trait hooks with defaults (chosen).** Each hook defaults to today's
   behaviour, so Coda and every existing test are unchanged; the echo backend overrides
   them. No owner-gate or routing rule changes.
3. **Echo-specific branches in the core.** Rejected: couples the core to one backend.

## Trait hooks (all defaulted)

| Hook | Default | Echo |
|---|---|---|
| `answer_timeout() -> Duration` | `CARD_TIMEOUT` (600 s) | `answerTimeoutS` |
| `activity_labels() -> ActivityLabels { working, question }` | `thinking`, `question` | `echoing`, `question open` |
| `skip_texts_older_than() -> Option<Duration>` | `None` | 60 s |
| `Capability::Sessions` now also gates `/new` and `/cd` | — | not reported |
| `/help` lists only commands the backend supports | Coda: unchanged list | `/status`, `/help` |

`skip_texts_older_than`: an owner text whose `server_ts` is more than that before the
bridge's start is dropped (logged `old_text_skipped`, no content). Only texts; card answers
are checked by the CardBook as before.

## Behaviour

- **Who may talk:** the core's rule is unchanged: only the agent's owner, in the owner's
  direct conversation; anyone else (accepted contacts included) gets "This agent is
  private." The asker is therefore always the owner; others' card answers are refused by
  the CardBook (sender check). Documented.
- **Echo:** any other text → turn: `TurnStarted`, `TextDelta("echo: <text>")`,
  `TurnEnded`. `text_format` Markdown → `agent.reply@2`; the core pages at 4,000
  characters, so long echoes span several cards. Activity `working` / `echoing`.
- **`ask`** (trimmed, any case): a `Question` request, options Red, Blue, single choice,
  free text allowed → `agent.question@1`; activity `waiting` / `question open`. The answer
  (button, label/number, or free text) → "you chose: <answer>".
- **`pick`**: multi-select Red, Blue, Green → `agent.choose@1` — the core's built-in
  multi-select template (seeded by message-svc as a built-in; `coda.choose@1` is not a
  built-in and would need publishing). "you chose: Red, Green".
- **Timeout:** the core's deadline is `answerTimeoutS`; it replies "No answer; skipped" and
  declines; the echo turn then ends silently.
- **Interrupt / stop:** the open question is declined and the turn ends (core says
  "Declined: the turn was stopped.").
- **Calls:** nothing echo-specific: with `calls.adapter`, the core answers the owner's
  call, sends each final transcript as a prompt and speaks the reply ("echo: <text>")
  through the adapter; without it, calls are not advertised.
- **Capabilities:** none. `/model`, `/tasks`, `/sessions`, `/new`, `/cd`, … answer "Not
  supported by Echo."; `/status`, `/help` and `/stop` are the core's.

## Tray

Content-free: the bridge's `status.backend.name` is already on the control channel. The
tray uses it for "Restart <name>", "<name> restarting" and "<name> version" (window JSON
`backendName`/`backendVersion`), falling back to "backend" before the first status.

## Tests

`core/echo_tests.rs` (core + `TestMessenger`, paused time): echo + activity, splitting,
ask/pick cards, stranger's answer ignored, free text answer, timeout at
`answerTimeoutS`, old text skipped, capabilities/commands/help; `voice_tests.rs`: an echo
call through the testkit adapter speaks "echo: run the tests". Config tests; tray-core
tests for the labels.
