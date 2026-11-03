# Voice fixtures

Recorded owner speech for the bridge's voice tests (`src/core/voice_tests.rs`,
`tests/bridge.rs`). Speech is tones: `voice::fake::FakeStt` hears a word per frequency
and `voice::fake::FakeTts` speaks a 600 Hz tone, so the fixtures survive real Opus media
in end-to-end tests too.

Each file is a `voice::fake::Fixture`:

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

- `words`: what `Fixture::stt()` hears: the strongest tone of an utterance is its text.
- `script`: the owner's audio (16 kHz mono, tones at amplitude 8000); `Fixture::audio()`.
  An utterance ends after 700 ms of silence (the detector's default), so each `say` is
  followed by at least 1000 ms of silence.

Frequencies lie between 350 and 2400 Hz, at least 200 Hz apart (also across fixtures, as
tests combine their words), and never within 150 Hz of 600 Hz, the fake TTS's tone: the
agent's voice must not be heard as the owner's words.

| Fixture | Words |
|---|---|
| `prompt.json` | "run the tests" (800 Hz) |
| `barge_in.json` | "stop, use the other file" (450 Hz) |
| `approve.json` | "approve" (1000 Hz) |
| `option_two.json` | "option two" (1300 Hz) |
| `ambiguous.json` | "maybe later" (1600 Hz), then "reject" (1900 Hz) |
