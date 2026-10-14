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
    ERROR_TYPE_URL, FrameError, InvalidId, MAX_FRAME_BYTES, decode_frame, encode_frame,
    error_from_status, error_status, id, id_bytes, kick_channel, notify_channel,
};

/// WebSocket close codes (spec §7.4). Codes from 4000 are the protocol's own; the others
/// are RFC 6455's. A client signs out only on 4001; it never reconnects after 4003; on
/// every other code it reconnects with backoff (after refreshing its token on 4002).
pub mod close {
    /// The access token is invalid (signature, audience, an unverified email) or older
    /// than the user's last password reset: sign in again.
    pub const INVALID_IDENTITY: u16 = 4001;
    /// The access token expired, names a key the gateway does not hold, or no `ReAuth`
    /// came by `exp + 30 s`: refresh it, then reconnect.
    pub const REAUTH_REQUIRED: u16 = 4002;
    /// Another connection of the device took over, or the device was revoked: stop.
    pub const TAKEN_OVER: u16 = 4003;
    /// Rate limited: back off, then reconnect.
    pub const RATE_LIMITED: u16 = 4004;
    /// The server is shutting down: back off with jitter, then reconnect.
    pub const SHUTTING_DOWN: u16 = 4005;
    /// The device was registered moments ago and the server is not ready for it yet:
    /// reconnect after a short backoff. Never a reason to sign out.
    pub const NOT_READY_YET: u16 = 4006;
    /// A protocol violation before `Ready`: no `Identify` within 5 s, or another frame
    /// first. Reconnect with backoff.
    pub const PROTOCOL_ERROR: u16 = 1002;
    /// The client sent no frame of any kind for two and a half heartbeats.
    pub const POLICY_VIOLATION: u16 = 1008;
    /// A message larger than the WebSocket limit.
    pub const MESSAGE_TOO_BIG: u16 = 1009;
    /// The server cannot serve the connection now (message-svc or the token keys are
    /// unreachable): reconnect with backoff.
    pub const INTERNAL_ERROR: u16 = 1011;
}
