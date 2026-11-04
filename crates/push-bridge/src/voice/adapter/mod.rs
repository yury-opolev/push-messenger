//! The voice adapter client: speech runs in a separate program, the voice adapter, that
//! the bridge starts ([`spawn`]) and talks to with push-voice-adapter/1
//! (docs/voice-adapter-protocol.md).

#[cfg(windows)]
mod job;
pub mod spawn;
