//! A scripted voice adapter for tests: it speaks `push-voice-adapter/1`
//! (docs/voice-adapter-protocol.md) like a real adapter, but its speech is tones.
//!
//! - It hears the caller's audio (stream 0) as words: each [`Word`] of its [`Script`] is a
//!   tone ([`detector`]); an utterance gets `speech_started` and a final `transcript` of
//!   its words.
//! - It speaks a `speak` as a tone at [`Script::speak_hz`] (600 Hz) for `ms_per_char` per
//!   character, in real time, then `speak_done`; `stop_speaking` cuts it.
//! - It plays faults on demand: a crash, a refused format, another protocol, a stall, a
//!   flood, no hello ([`Script`]).
//! - It records what it received and sent, with when, in a [`Log`].
//! - Its recorded [`fixture`]s (`fixtures/*.json`) script a caller's speech as tones.
//!
//! Use it in-process ([`ScriptedAdapter`], [`duplex`]) or as the
//! `push-voice-adapter-testkit` binary (spawn or connect mode).

mod adapter;
pub mod detector;
pub mod fixture;
pub mod log;
pub mod script;
pub mod tones;

use tokio::io::{AsyncRead, AsyncWrite, DuplexStream};
use tokio::task::JoinHandle;

pub use fixture::Fixture;
pub use log::{Event, Log, Observed};
pub use script::{Script, Word};

/// Bytes each direction of [`duplex`] buffers: several seconds of audio, so a test that
/// writes a fixture at once never blocks on the adapter.
pub const DUPLEX_BUFFER: usize = 1 << 20;

/// How a connection ended without a fault of ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The bridge closed the stream.
    Closed,
    /// We refused the bridge's hello (`unsupported_format`, `unsupported_protocol`, a
    /// malformed hello) and closed.
    Refused,
}

/// A connection that ended badly.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// The `crash_after_frames` fault: the stream was dropped without a proper close.
    #[error("crashed as scripted")]
    Crashed,
    #[error(transparent)]
    Read(#[from] push_voice_adapter_proto::ReadError),
    #[error(transparent)]
    Write(#[from] push_voice_adapter_proto::WriteError),
}

/// A scripted adapter and its log.
#[derive(Debug)]
pub struct ScriptedAdapter {
    script: Script,
    log: Log,
}

impl ScriptedAdapter {
    pub fn new(script: Script) -> Self {
        Self::with_log(script, Log::new())
    }

    /// An adapter recording into `log` (shared with its clones).
    pub fn with_log(script: Script, log: Log) -> Self {
        Self { script, log }
    }

    /// Its log.
    pub fn log(&self) -> Log {
        self.log.clone()
    }

    /// Serves one bridge connection: `reader` carries the bridge's frames, `writer` ours.
    /// Returns when the bridge closes the stream or a fault ends it; never with
    /// `stall_after_hello`.
    pub async fn serve<R, W>(&self, reader: R, writer: W) -> Result<Outcome, AdapterError>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        adapter::serve(&self.script, &self.log, reader, writer).await
    }

    /// [`serve`](Self::serve) with a fresh adapter for `script`.
    pub async fn run<R, W>(reader: R, writer: W, script: Script) -> Result<Outcome, AdapterError>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        Self::new(script).serve(reader, writer).await
    }
}

/// A scripted adapter running in-process on a tokio task, and the bridge's ends.
#[derive(Debug)]
pub struct InProcess {
    /// The adapter's frames, for the bridge to read.
    pub reader: DuplexStream,
    /// For the bridge to write its frames; dropping it closes the adapter's input.
    pub writer: DuplexStream,
    /// What the adapter saw and did.
    pub log: Log,
    /// The adapter's [`ScriptedAdapter::serve`].
    pub task: JoinHandle<Result<Outcome, AdapterError>>,
}

/// Starts `script` on a tokio task over in-memory pipes (needs a runtime).
pub fn duplex(script: Script) -> InProcess {
    let (bridge_reader, adapter_writer) = tokio::io::duplex(DUPLEX_BUFFER);
    let (adapter_reader, bridge_writer) = tokio::io::duplex(DUPLEX_BUFFER);
    let adapter = ScriptedAdapter::new(script);
    let log = adapter.log();
    let task = tokio::spawn(async move { adapter.serve(adapter_reader, adapter_writer).await });
    InProcess {
        reader: bridge_reader,
        writer: bridge_writer,
        log,
        task,
    }
}
