//! The agent bridge's local control channel (docs/agent-bridge.md): the wire protocol
//! ([`protocol`]), the instance id and endpoint naming ([`endpoint`]), and the per-user
//! transport ([`transport`]).

pub mod endpoint;
pub mod protocol;
pub mod transport;

pub use endpoint::{Endpoint, endpoint, endpoint_for, instance_id};
pub use protocol::*;
