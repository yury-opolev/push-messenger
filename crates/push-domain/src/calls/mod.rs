//! 1:1 audio calls: input checks, the call state machine and who may call whom
//! (docs/superpowers/specs/2026-11-03-voice-calls-design.md).

mod sdp;

pub use sdp::{
    CallInputError, CandidateInput, MAX_CANDIDATE_BYTES, MAX_SDP_BYTES, MAX_SDP_MID_BYTES,
    MAX_UFRAG_BYTES, check_candidate, check_sdp,
};
