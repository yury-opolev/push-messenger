//! The agent bridge's local control channel (docs/agent-bridge.md): the wire protocol
//! ([`protocol`]) and the instance id and endpoint naming ([`endpoint`]).

pub mod endpoint;
pub mod protocol;

pub use endpoint::{Endpoint, endpoint, endpoint_for, instance_id};
pub use protocol::*;
