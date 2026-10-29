//! The Coda backend: drives `coda serve` over stdio with JSON-RPC 2.0 and LSP-style
//! Content-Length framing.

pub mod framing;
pub mod rpc;
