//! The Coda backend: drives `coda serve` over stdio with JSON-RPC 2.0 and LSP-style
//! Content-Length framing.

pub mod backend;
pub mod framing;
pub mod rpc;
#[doc(hidden)]
pub mod testing;

pub use backend::{CodaBackend, Launcher, ProcessLauncher, Spawned};
