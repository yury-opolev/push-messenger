//! What the SDK hands to apps about 1:1 audio calls (docs/calls.md): the signalling events
//! and the values they carry. The app runs the media (WebRTC) itself; the SDK carries the
//! SDP and ICE candidates.
//!
//! Times are milliseconds since the Unix epoch. `Debug` never shows an SDP, an ICE
//! candidate or a username fragment (they carry addresses): only their length.

use push_domain::{CallId, DeviceId, UserId};
use push_proto::v1;
use uuid::Uuid;

use crate::SdkError;
use crate::model::{Redacted, redacted};

/// Whether an SDP is an offer or an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SdpKind {
    Offer,
    Answer,
}

impl SdpKind {
    pub(crate) fn to_wire(self) -> v1::CallSdpType {
        match self {
            Self::Offer => v1::CallSdpType::Offer,
            Self::Answer => v1::CallSdpType::Answer,
        }
    }

    pub(crate) fn from_wire(kind: v1::CallSdpType) -> Option<Self> {
        match kind {
            v1::CallSdpType::Offer => Some(Self::Offer),
            v1::CallSdpType::Answer => Some(Self::Answer),
            v1::CallSdpType::Unspecified => None,
        }
    }
}

/// One trickled ICE candidate (an `RTCIceCandidate`).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct IceCandidate {
    /// The candidate line, starting with `candidate:`; empty with `end_of_candidates`.
    pub candidate: String,
    /// The media stream identification tag.
    pub sdp_mid: Option<String>,
    /// The index of the `m=` line the candidate belongs to.
    pub sdp_mline_index: Option<u32>,
    /// The ICE username fragment the candidate belongs to.
    pub username_fragment: Option<String>,
    /// No further candidates of this side for this generation.
    pub end_of_candidates: bool,
}

impl IceCandidate {
    /// The marker sent after the last candidate: no candidate line, the flag set.
    pub fn end_of_candidates() -> Self {
        Self {
            end_of_candidates: true,
            ..Self::default()
        }
    }

    pub(crate) fn from_wire(ice: v1::CallIce) -> Self {
        Self {
            candidate: ice.candidate,
            sdp_mid: ice.sdp_mid,
            sdp_mline_index: ice.sdp_mline_index,
            username_fragment: ice.username_fragment,
            end_of_candidates: ice.end_of_candidates,
        }
    }
}

impl std::fmt::Debug for IceCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IceCandidate")
            .field("candidate", &Redacted(self.candidate.len()))
            .field("sdp_mid", &self.sdp_mid)
            .field("sdp_mline_index", &self.sdp_mline_index)
            .field("username_fragment", &redacted(&self.username_fragment))
            .field("end_of_candidates", &self.end_of_candidates)
            .finish()
    }
}

/// Who called whom, as the user sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallDirection {
    /// The user is the callee.
    Incoming,
    /// The user is the caller.
    Outgoing,
}

/// Where a call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallStatus {
    /// Not answered yet.
    Ringing,
    /// Answered by a callee device.
    Active,
}

/// Why a call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CallEndReason {
    /// A party hung up after the call was answered.
    Completed,
    /// Nobody answered in time.
    Missed,
    /// A ringing callee device declined.
    Declined,
    /// The caller cancelled before anyone answered.
    Cancelled,
    /// The callee is in another call, or declined as busy.
    Busy,
    /// The call broke: a party reported a failure, or a party's device went away. Also
    /// any reason this client does not know.
    Failed,
    /// The callee has no calls-capable device online.
    Unreachable,
}

impl CallEndReason {
    /// `"completed"`, `"missed"`, `"declined"`, `"cancelled"`, `"busy"`, `"failed"` or
    /// `"unreachable"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Missed => "missed",
            Self::Declined => "declined",
            Self::Cancelled => "cancelled",
            Self::Busy => "busy",
            Self::Failed => "failed",
            Self::Unreachable => "unreachable",
        }
    }

    pub(crate) fn from_wire(reason: v1::CallEndReason) -> Self {
        match reason {
            v1::CallEndReason::Completed => Self::Completed,
            v1::CallEndReason::Missed => Self::Missed,
            v1::CallEndReason::Declined => Self::Declined,
            v1::CallEndReason::Cancelled => Self::Cancelled,
            v1::CallEndReason::Busy => Self::Busy,
            v1::CallEndReason::Unreachable => Self::Unreachable,
            v1::CallEndReason::Failed | v1::CallEndReason::Unspecified => Self::Failed,
        }
    }
}

/// Why the server refused a call frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CallErrorCode {
    /// Accept or cancel after a callee device answered. A caller that gets it for a
    /// cancel has a connected call: hang up.
    AlreadyAnswered,
    /// No such call for this device in this role: drop the call locally.
    CallNotFound,
    /// The user already has a call on one of their devices.
    InCall,
    /// The call's cap of ICE candidates or renegotiations is reached.
    CallLimit,
    /// The callee is not an accepted contact (or is blocked either way).
    NotContact,
    /// The frame broke a rule.
    Invalid,
    /// Too many invites: see the error's `retry_after_s`.
    RateLimited,
    /// Calls are not offered on this connection.
    NotAvailable,
    /// The server could not handle it now: the same frame may succeed later.
    Unavailable,
    /// A code this client does not know.
    Other,
}

impl CallErrorCode {
    pub(crate) fn from_wire(code: v1::ErrorCode) -> Self {
        match code {
            v1::ErrorCode::AlreadyAnswered => Self::AlreadyAnswered,
            v1::ErrorCode::CallNotFound => Self::CallNotFound,
            v1::ErrorCode::InCall => Self::InCall,
            v1::ErrorCode::CallLimit => Self::CallLimit,
            v1::ErrorCode::NotContact => Self::NotContact,
            v1::ErrorCode::Invalid => Self::Invalid,
            v1::ErrorCode::RateLimited => Self::RateLimited,
            v1::ErrorCode::NotAvailable => Self::NotAvailable,
            v1::ErrorCode::Unavailable => Self::Unavailable,
            _ => Self::Other,
        }
    }
}

/// One of the user's calls as the server sees it, listed after every (re)connect.
#[derive(Clone, PartialEq, Eq)]
pub struct CallState {
    pub call_id: CallId,
    /// The other party.
    pub peer_user_id: UserId,
    pub direction: CallDirection,
    pub status: CallStatus,
    /// An incoming ringing call only: the caller's SDP offer, to answer it.
    pub offer_sdp: Option<String>,
    /// Ringing only: when ringing times out, on this device's clock (receipt time plus
    /// what the server said was left).
    pub expires_at_ms: Option<i64>,
    /// Active only: when the call was answered (server clock).
    pub answered_at_ms: Option<i64>,
    /// The call's device on this side is this device: the caller's device for an
    /// outgoing call, the answering device for an active incoming one, or a ringing one.
    pub this_device: bool,
}

impl CallState {
    /// An active call on this device after a reconnect: restart ICE with
    /// [`crate::Calls::renegotiate`] (an offer with `iceRestart`).
    pub fn needs_ice_restart(&self) -> bool {
        self.status == CallStatus::Active && self.this_device
    }

    /// `None` when it names no call or user, or no direction or status.
    pub(crate) fn from_wire(state: v1::CallState, now_ms: i64) -> Option<Self> {
        let direction = match state.direction() {
            v1::CallDirection::In => CallDirection::Incoming,
            v1::CallDirection::Out => CallDirection::Outgoing,
            v1::CallDirection::Unspecified => return None,
        };
        let status = match state.status() {
            v1::CallStatus::Ringing => CallStatus::Ringing,
            v1::CallStatus::Active => CallStatus::Active,
            v1::CallStatus::Unspecified => return None,
        };
        Some(Self {
            call_id: CallId(Uuid::from_slice(&state.call_id).ok()?),
            peer_user_id: UserId(Uuid::from_slice(&state.peer_user_id).ok()?),
            direction,
            status,
            offer_sdp: state.offer_sdp,
            expires_at_ms: state
                .expires_in_ms
                .map(|left| now_ms.saturating_add(i64::from(left))),
            answered_at_ms: state.answered_at.map(|micros| micros.div_euclid(1_000)),
            this_device: state.this_device,
        })
    }
}

impl std::fmt::Debug for CallState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CallState")
            .field("call_id", &self.call_id)
            .field("peer_user_id", &self.peer_user_id)
            .field("direction", &self.direction)
            .field("status", &self.status)
            .field("offer_sdp", &redacted(&self.offer_sdp))
            .field("expires_at_ms", &self.expires_at_ms)
            .field("answered_at_ms", &self.answered_at_ms)
            .field("this_device", &self.this_device)
            .finish()
    }
}

/// What happened to a call, as [`crate::Event::Call`]. Only on a connection where calls
/// are available ([`crate::Calls::available`]); not stored.
#[derive(Clone, PartialEq, Eq)]
pub enum CallEvent {
    /// A call rings on this device: ring until `expires_at_ms` (this device's clock)
    /// unless it is taken or ends first.
    Incoming {
        call_id: CallId,
        caller_id: UserId,
        offer_sdp: String,
        expires_at_ms: i64,
    },
    /// The callee answered the user's call on `answered_device_id`: apply the answer.
    Accepted {
        call_id: CallId,
        answer_sdp: String,
        answered_device_id: DeviceId,
    },
    /// Another device of the user answered: stop ringing here.
    Taken { call_id: CallId },
    /// The call is over: release its media. `duration_s` for calls that were answered.
    Ended {
        call_id: CallId,
        reason: CallEndReason,
        duration_s: Option<u32>,
    },
    /// An ICE candidate of the other party.
    PeerIce {
        call_id: CallId,
        candidate: IceCandidate,
    },
    /// A new offer or answer from the other party (an ICE restart, for one).
    PeerRenegotiate {
        call_id: CallId,
        sdp: String,
        kind: SdpKind,
    },
    /// The server refused a call frame this device sent. `error` is an
    /// [`SdkError::Server`] with the server's code (`already_answered`, `in_call`, …).
    Error {
        call_id: CallId,
        code: CallErrorCode,
        error: SdkError,
    },
    /// The user's calls, after every (re)connect. `gone` are the calls this client knew
    /// as live that are not listed: drop them. Restart ICE for the listed calls whose
    /// [`CallState::needs_ice_restart`].
    Synced {
        calls: Vec<CallState>,
        gone: Vec<CallId>,
    },
    /// The user's call history changed: refresh it. `latest_ended_at_ms` is when the
    /// call that ended did (server clock); `None` when a call was hidden.
    HistoryChanged { latest_ended_at_ms: Option<i64> },
}

impl std::fmt::Debug for CallEvent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Incoming {
                call_id,
                caller_id,
                offer_sdp,
                expires_at_ms,
            } => formatter
                .debug_struct("Incoming")
                .field("call_id", call_id)
                .field("caller_id", caller_id)
                .field("offer_sdp", &Redacted(offer_sdp.len()))
                .field("expires_at_ms", expires_at_ms)
                .finish(),
            Self::Accepted {
                call_id,
                answer_sdp,
                answered_device_id,
            } => formatter
                .debug_struct("Accepted")
                .field("call_id", call_id)
                .field("answer_sdp", &Redacted(answer_sdp.len()))
                .field("answered_device_id", answered_device_id)
                .finish(),
            Self::Taken { call_id } => formatter
                .debug_struct("Taken")
                .field("call_id", call_id)
                .finish(),
            Self::Ended {
                call_id,
                reason,
                duration_s,
            } => formatter
                .debug_struct("Ended")
                .field("call_id", call_id)
                .field("reason", reason)
                .field("duration_s", duration_s)
                .finish(),
            Self::PeerIce { call_id, candidate } => formatter
                .debug_struct("PeerIce")
                .field("call_id", call_id)
                .field("candidate", candidate)
                .finish(),
            Self::PeerRenegotiate { call_id, sdp, kind } => formatter
                .debug_struct("PeerRenegotiate")
                .field("call_id", call_id)
                .field("sdp", &Redacted(sdp.len()))
                .field("kind", kind)
                .finish(),
            Self::Error {
                call_id,
                code,
                error,
            } => formatter
                .debug_struct("Error")
                .field("call_id", call_id)
                .field("code", code)
                .field("error", error)
                .finish(),
            Self::Synced { calls, gone } => formatter
                .debug_struct("Synced")
                .field("calls", calls)
                .field("gone", gone)
                .finish(),
            Self::HistoryChanged { latest_ended_at_ms } => formatter
                .debug_struct("HistoryChanged")
                .field("latest_ended_at_ms", latest_ended_at_ms)
                .finish(),
        }
    }
}
