//! The voice adapter client: speech runs in a separate program, the voice adapter, that
//! the bridge starts ([`spawn`]) or connects to ([`connect`]) and talks to with
//! push-voice-adapter/1 (docs/voice-adapter-protocol.md). [`advertise`] decides when
//! calls are advertised; [`report`] describes the adapter for `status` and the tray.

pub mod advertise;
pub mod connect;
#[cfg(windows)]
mod job;
pub mod link;
pub mod report;
pub mod router;
pub mod spawn;
pub mod supervisor;
