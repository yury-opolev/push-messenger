# Voice fixtures

Recorded owner speech for voice tests: push-bridge's core tests
(`crates/push-bridge/src/core/voice_tests.rs`), its integration tests
(`crates/push-bridge/tests/bridge.rs`) and the end-to-end tests through real media
(`crates/push-smoke/tests/bridge_voice*.rs`). This crate, the scripted voice adapter, owns
them and their rules: speech is tones. The adapter hears a word per frequency (its
`detector`) and speaks a 600 Hz tone, so the fixtures survive real Opus media too.

Each file is a `push_voice_adapter_testkit::Fixture` (`Fixture::named("prompt")`):

```json
{
  "description": "what the owner does",
  "words": [{ "hz": 1000, "text": "approve" }],
  "script": [
    { "silence": { "ms": 200 } },
    { "say": { "hz": 1000, "ms": 900 } },
    { "silence": { "ms": 1000 } }
  ]
}
```

- `words`: what the adapter hears, a `Word` each (`text` or `word`): give them to a
  `Script` (`Script::with_words(fixture::all_words())`).
- `script`: the owner's audio (16 kHz mono, tones at amplitude 8000, whole 20 ms frames);
  `Fixture::audio()`. Send it in real time (one frame per 20 ms): the bridge keeps at most
  1 s of the owner's audio queued for the adapter.

The rules (`Script::check` enforces them; a test checks every fixture):

- Frequencies lie between 350 and 2400 Hz, at least 200 Hz apart (also across fixtures, as
  tests combine their words), and never within 150 Hz of 600 Hz, the adapter's voice: the
  agent's voice must not be heard as the owner's words.
- A word counts once its tone sounds for 3 frames (60 ms) in a row; an utterance ends
  after 15 frames (300 ms) without a word, and its transcript is its words in order. So
  each `say` is followed by at least 1000 ms of silence: one utterance per `say`, with
  room to spare.

| Fixture | Words |
|---|---|
| `prompt.json` | "run the tests" (800 Hz) |
| `barge_in.json` | "stop, use the other file" (450 Hz) |
| `approve.json` | "approve" (1000 Hz) |
| `option_two.json` | "option two" (1300 Hz) |
| `ambiguous.json` | "maybe later" (1600 Hz), then "reject" (1900 Hz) |