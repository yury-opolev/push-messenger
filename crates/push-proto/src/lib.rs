//! The push-messenger protocols as protobuf (spec §7): the client wire protocol
//! ([`v1`], one [`v1::Frame`] per WebSocket binary message) and the internal gRPC API of
//! message-svc ([`internal::v1`]). The `.proto` files under `proto/` are the contract; the
//! Rust code is generated at build time with a vendored `protoc`.
//!
//! Identifiers travel as 16-byte UUIDs ([`id`], [`id_bytes`]); times as microseconds since
//! the Unix epoch.

pub mod v1 {
    //! The client wire protocol, package `push.v1`.
    tonic::include_proto!("push.v1");
}

pub mod internal {
    //! The internal API between the gateway and message-svc.
    pub mod v1 {
        //! Package `push.internal.v1`.
        tonic::include_proto!("push.internal.v1");
    }
}

mod codec;

pub use codec::{
    FrameError, InvalidId, MAX_FRAME_BYTES, decode_frame, encode_frame, error_from_status,
    error_status, id, id_bytes, notify_channel,
};

/// WebSocket close codes (spec §7.4).
pub mod close {
    /// Invalid token or identity, unverified email, or unknown or revoked device: sign in
    /// again.
    pub const INVALID_IDENTITY: u16 = 4001;
    /// Re-authentication required: refresh, then reconnect.
    pub const REAUTH_REQUIRED: u16 = 4002;
    /// Another connection took over, or the device was revoked: stop.
    pub const TAKEN_OVER: u16 = 4003;
    /// Rate limited: back off, then reconnect.
    pub const RATE_LIMITED: u16 = 4004;
    /// The server is shutting down: back off with jitter, then reconnect.
    pub const SHUTTING_DOWN: u16 = 4005;
}
