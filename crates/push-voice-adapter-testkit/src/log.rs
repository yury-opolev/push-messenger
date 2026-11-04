//! What a scripted adapter saw and did, with when: for tests' assertions and timing
//! (`final transcript sent → speak received`). Content-free like the protocol's logs:
//! lengths, never transcript or speech text.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::Notify;
use tokio::time::Instant;

/// One thing the adapter received (`*_received`, or the bridge's message name) or sent
/// (`*_sent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Connect mode: a bridge connected and is served.
    Connected,
    /// Connect mode: a second bridge connected while one is served; it got `busy`.
    RejectedBusy,
    /// The bridge's hello: the versions it offered and its language.
    HelloReceived {
        protocols: Vec<String>,
        language: String,
    },
    /// Our hello, with the protocol we answered.
    HelloSent {
        protocol: String,
    },
    CallStarted {
        call_id: String,
    },
    /// A stream-0 frame of the active call.
    Audio {
        call_id: String,
        seq: u32,
        samples: usize,
    },
    Speak {
        call_id: String,
        speak_id: String,
        interruptible: bool,
        chars: usize,
    },
    StopSpeaking {
        call_id: String,
        speak_id: Option<String>,
    },
    CallEnded {
        call_id: String,
        reason: String,
    },
    /// An `error` from the bridge.
    ErrorReceived {
        code: String,
    },
    PingReceived {
        id: u32,
    },
    SpeechStartedSent {
        call_id: String,
        utterance_id: String,
    },
    TranscriptSent {
        call_id: String,
        utterance_id: String,
        #[serde(rename = "final")]
        is_final: bool,
        chars: usize,
    },
    /// A speak's first stream-1 frame.
    SpeakAudioStarted {
        call_id: String,
        speak_id: String,
    },
    SpeakDoneSent {
        call_id: String,
        speak_id: String,
        interrupted: bool,
        /// Stream-1 frames sent for it.
        frames: usize,
    },
    ErrorSent {
        code: String,
    },
    /// The `crash_after_frames` fault fired.
    Crashed {
        frames: u64,
    },
    /// The bridge closed the stream.
    Closed,
}

/// An [`Event`] and when it happened (tokio's clock: paused time works).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub at: Instant,
    pub event: Event,
}

#[derive(Serialize)]
struct Line<'a> {
    /// Microseconds since the log was made.
    t_us: u128,
    #[serde(flatten)]
    event: &'a Event,
}

struct Inner {
    start: Instant,
    events: Mutex<Vec<Observed>>,
    file: Mutex<Option<File>>,
    changed: Notify,
}

/// A shared, append-only log of [`Observed`] events; cloning shares it.
#[derive(Clone)]
pub struct Log(Arc<Inner>);

impl Default for Log {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Log {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Log({} events)", self.len())
    }
}

impl Log {
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            start: Instant::now(),
            events: Mutex::new(Vec::new()),
            file: Mutex::new(None),
            changed: Notify::new(),
        }))
    }

    /// A log that also appends each event to `path` as a JSON line
    /// `{"t_us": <µs since start>, "event": "<name>", …fields}`.
    pub fn with_file(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let log = Self::new();
        *log.0.file.lock().expect("the log is not poisoned") = Some(file);
        Ok(log)
    }

    /// Records an event now.
    pub fn push(&self, event: Event) {
        let at = Instant::now();
        {
            let mut file = self.0.file.lock().expect("the log is not poisoned");
            if let Some(file) = file.as_mut() {
                let line = Line {
                    t_us: at.duration_since(self.0.start).as_micros(),
                    event: &event,
                };
                if let Ok(mut json) = serde_json::to_vec(&line) {
                    json.push(b'\n');
                    let _ = file.write_all(&json);
                }
            }
        }
        self.0
            .events
            .lock()
            .expect("the log is not poisoned")
            .push(Observed { at, event });
        self.0.changed.notify_waiters();
    }

    /// Every event so far.
    pub fn events(&self) -> Vec<Observed> {
        self.0
            .events
            .lock()
            .expect("the log is not poisoned")
            .clone()
    }

    pub fn len(&self) -> usize {
        self.0.events.lock().expect("the log is not poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The first event so far that `matches`.
    pub fn find(&self, matches: impl Fn(&Event) -> bool) -> Option<Observed> {
        self.0
            .events
            .lock()
            .expect("the log is not poisoned")
            .iter()
            .find(|observed| matches(&observed.event))
            .cloned()
    }

    /// How many events so far `match`.
    pub fn count(&self, matches: impl Fn(&Event) -> bool) -> usize {
        self.0
            .events
            .lock()
            .expect("the log is not poisoned")
            .iter()
            .filter(|observed| matches(&observed.event))
            .count()
    }

    /// Waits for the first event that `matches` (one already logged returns at once).
    /// Wrap it in a timeout.
    pub async fn wait_for(&self, matches: impl Fn(&Event) -> bool) -> Observed {
        loop {
            let changed = self.0.changed.notified();
            if let Some(found) = self.find(&matches) {
                return found;
            }
            changed.await;
        }
    }

    /// Waits until `count(matches) >= n`. Wrap it in a timeout.
    pub async fn wait_count(&self, n: usize, matches: impl Fn(&Event) -> bool) {
        loop {
            let changed = self.0.changed.notified();
            if self.count(&matches) >= n {
                return;
            }
            changed.await;
        }
    }
}
