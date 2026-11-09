//! The call state machine (spec §3): a pure function from a call record and an event to
//! the next record and what to publish. No I/O and no clock: `now_ms` is Redis time,
//! given by the caller. The service stores the record with compare-and-set and publishes
//! the deliveries.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ids::{DeviceId, UserId};

/// At most this many `CallIce` frames per side per call (spec D15).
pub const MAX_ICE_PER_SIDE: u32 = 200;
/// At most this many caller candidates buffered before the answer (spec D8).
pub const MAX_PENDING_ICE: usize = 32;
/// At most this many `CallRenegotiate` frames per side per call (spec D15).
pub const MAX_RENEGOTIATIONS: u32 = 20;

/// The record format written in `v`.
pub const RECORD_VERSION: u32 = 1;

/// A live call's state. An ended call has no record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    Ringing,
    Active,
}

/// Why a call ended (`CallEndReason` on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Completed,
    Missed,
    Declined,
    Cancelled,
    Busy,
    Failed,
    Unreachable,
}

impl EndReason {
    /// The reason's name in audit lines, metric labels and call history.
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
}

/// A party of a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Caller,
    Callee,
}

/// A trickled ICE candidate, relayed as is (checked by `check_candidate` before).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Candidate {
    #[serde(default)]
    pub candidate: String,
    #[serde(default)]
    pub sdp_mid: Option<String>,
    #[serde(default)]
    pub sdp_mline_index: Option<u32>,
    #[serde(default)]
    pub end_of_candidates: bool,
    #[serde(default)]
    pub username_fragment: Option<String>,
}

/// A live call (spec §3.1), stored as JSON. Unknown fields are ignored and missing ones
/// default, so service versions read each other's records during a rollout (spec D19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRecord {
    pub v: u32,
    pub call_id: Uuid,
    pub caller: UserId,
    pub caller_device: DeviceId,
    pub callee: UserId,
    pub state: CallState,
    /// The offer, kept while ringing for `CallSync`; dropped once answered.
    #[serde(default)]
    pub offer: Option<String>,
    /// The callee devices that may accept or decline while ringing; empty once answered.
    #[serde(default)]
    pub ringing_devices: Vec<DeviceId>,
    /// The answering device, once active.
    #[serde(default)]
    pub callee_device: Option<DeviceId>,
    pub created_ms: i64,
    pub ring_deadline_ms: i64,
    #[serde(default)]
    pub answered_ms: Option<i64>,
    /// When the caller's device was found gone; `None` while connected.
    #[serde(default)]
    pub caller_away_ms: Option<i64>,
    /// When the answering device was found gone; `None` while connected.
    #[serde(default)]
    pub callee_away_ms: Option<i64>,
    #[serde(default)]
    pub ice_caller: u32,
    #[serde(default)]
    pub ice_callee: u32,
    #[serde(default)]
    pub reneg_caller: u32,
    #[serde(default)]
    pub reneg_callee: u32,
    /// Caller candidates buffered before the answer, in arrival order.
    #[serde(default)]
    pub pending_ice: Vec<Candidate>,
    /// The caller is an agent (spec 2026-11-09 proactive agents D13); the call's history
    /// rows are marked `started_by_agent`.
    #[serde(default)]
    pub by_agent: bool,
    /// The next deadline: when the sweeper ticks the call.
    pub due_ms: i64,
}

/// The configured durations, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// How long a call rings (`calls.ringSeconds`).
    pub ring_ms: i64,
    /// How long a party device may be gone before the call fails
    /// (`calls.disconnectGraceSeconds`).
    pub grace_ms: i64,
    /// How often an active call checks that its party devices are still connected.
    pub liveness_ms: i64,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ring_ms: 45_000,
            grace_ms: 30_000,
            liveness_ms: 15_000,
        }
    }
}

/// What happened to a call (spec §3.2). `Invite` is [`start`].
///
/// Deviation from the plan: `Attach` and `Detach` carry the device's user, because the
/// record does not know which devices belong to the callee (an attaching callee device
/// joins the ringing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallEvent {
    /// `CallAccept`.
    Accept { device: DeviceId, answer: String },
    /// `CallDecline`; `busy` for `CALL_END_REASON_BUSY`.
    Decline { device: DeviceId, busy: bool },
    /// `CallCancel`.
    Cancel { device: DeviceId },
    /// `CallHangup`; `failed` for `CALL_END_REASON_FAILED`.
    Hangup { device: DeviceId, failed: bool },
    /// `CallIce`.
    Ice {
        device: DeviceId,
        candidate: Candidate,
    },
    /// `CallRenegotiate`; `offer` for an offer, else an answer.
    Renegotiate {
        device: DeviceId,
        sdp: String,
        offer: bool,
    },
    /// A calls-active connection of `device` (of `user`) attached.
    Attach { user: UserId, device: DeviceId },
    /// The registered connection of `device` (of `user`) detached.
    Detach { user: UserId, device: DeviceId },
    /// The call's due time passed. `gone` lists the sides whose party device the liveness
    /// read found without a live connection, `live` those it found connected (an active
    /// call clears their away time before checking the grace: the device came back).
    Tick { gone: Vec<Side>, live: Vec<Side> },
}

/// Why a frame was refused, answered with `CallError` to the sender only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// `CALL_NOT_FOUND`: the sender's device has no role in the call for this frame.
    NotFound,
    /// `ALREADY_ANSWERED`: an accept or cancel after the call was answered.
    AlreadyAnswered,
    /// `CALL_LIMIT`: the ICE or renegotiation cap is reached.
    Limit,
}

/// A frame to publish, and the devices that get it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// `CallIncoming` to these callee devices (built from the record).
    Incoming { to: Vec<DeviceId> },
    /// `CallAccepted` to the caller's device.
    Accepted {
        to: DeviceId,
        answer: String,
        device: DeviceId,
    },
    /// `CallTaken` to the callee's other ringing devices.
    Taken { to: Vec<DeviceId> },
    /// `CallEnded` to these devices of `user`.
    Ended {
        user: UserId,
        to: Vec<DeviceId>,
        reason: EndReason,
        duration_s: Option<u32>,
    },
    /// `peer_call_ice` to the other party's device.
    Ice {
        user: UserId,
        to: DeviceId,
        candidate: Candidate,
    },
    /// `peer_call_renegotiate` to the other party's device.
    Renegotiate {
        user: UserId,
        to: DeviceId,
        sdp: String,
        offer: bool,
    },
}

/// The result of an accepted event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The next record; `None` when the call ended.
    pub record: Option<CallRecord>,
    pub deliveries: Vec<Delivery>,
    pub ended: Option<EndReason>,
    /// The event answered the call.
    pub answered: bool,
    /// Whether the record changed (an ended call always did). The service skips the write
    /// otherwise, but still publishes `deliveries` (a repeated accept has one).
    pub changed: bool,
}

/// Whether a call is incoming or outgoing for the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallDirection {
    In,
    Out,
}

/// A call as one device sees it (`CallState` in `CallSync`, spec §4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallView {
    pub call_id: Uuid,
    pub peer: UserId,
    pub direction: CallDirection,
    pub status: CallState,
    /// The offer: for the callee while ringing.
    pub offer: Option<String>,
    /// The ring deadline, while ringing.
    pub expires_at_ms: Option<i64>,
    /// Milliseconds left to ring, while ringing.
    pub expires_in_ms: Option<u32>,
    /// When the call was answered, once active.
    pub answered_ms: Option<i64>,
    /// Whether this device is the call's device on its side: the caller's device, a
    /// ringing callee device, or the answering device.
    pub this_device: bool,
}

/// A new ringing call (the `Invite, ok` row of spec §3.3): the record and the
/// `CallIncoming` delivery to `ringing` devices. The service checks the policy, the input,
/// the busy locks and reachability before.
#[allow(clippy::too_many_arguments)]
pub fn start(
    call_id: Uuid,
    caller: UserId,
    caller_device: DeviceId,
    callee: UserId,
    offer: String,
    ringing: Vec<DeviceId>,
    now_ms: i64,
    timing: &Timing,
) -> (CallRecord, Vec<Delivery>) {
    let ring_deadline_ms = now_ms + timing.ring_ms;
    let deliveries = vec![Delivery::Incoming {
        to: ringing.clone(),
    }];
    let record = CallRecord {
        v: RECORD_VERSION,
        call_id,
        caller,
        caller_device,
        callee,
        state: CallState::Ringing,
        offer: Some(offer),
        ringing_devices: ringing,
        callee_device: None,
        created_ms: now_ms,
        ring_deadline_ms,
        answered_ms: None,
        caller_away_ms: None,
        callee_away_ms: None,
        ice_caller: 0,
        ice_callee: 0,
        reneg_caller: 0,
        reneg_callee: 0,
        pending_ice: Vec::new(),
        by_agent: false,
        due_ms: ring_deadline_ms,
    };
    (record, deliveries)
}

/// Applies `event` to `record` at `now_ms` (spec §3.3). `sender` is the user whose frame
/// it is for frame events (`Accept` … `Renegotiate`; `None` refuses them), and is ignored
/// for `Attach`, `Detach` and `Tick`. A refusal leaves the record as it is.
pub fn transition(
    record: &CallRecord,
    sender: Option<UserId>,
    event: CallEvent,
    now_ms: i64,
    timing: &Timing,
) -> Result<Outcome, Refusal> {
    match event {
        CallEvent::Attach { user, device } => Ok(attach(record, user, device, now_ms, timing)),
        CallEvent::Detach { user, device } => Ok(detach(record, user, device, now_ms, timing)),
        CallEvent::Tick { gone, live } => Ok(tick(record, &gone, &live, now_ms, timing)),
        frame => {
            let sender = sender.ok_or(Refusal::NotFound)?;
            on_frame(record, sender, frame, now_ms, timing)
        }
    }
}

/// How `device` of `user` sees the call; `None` if `user` is not a participant.
pub fn sync_view(
    record: &CallRecord,
    user: UserId,
    device: DeviceId,
    now_ms: i64,
) -> Option<CallView> {
    let ringing = record.state == CallState::Ringing;
    let (peer, direction, this_device) = if user == record.caller {
        (
            record.callee,
            CallDirection::Out,
            device == record.caller_device,
        )
    } else if user == record.callee {
        let this_device = if ringing {
            record.ringing_devices.contains(&device)
        } else {
            record.callee_device == Some(device)
        };
        (record.caller, CallDirection::In, this_device)
    } else {
        return None;
    };
    let offer = if ringing && direction == CallDirection::In {
        record.offer.clone()
    } else {
        None
    };
    let (expires_at_ms, expires_in_ms) = if ringing {
        let left = (record.ring_deadline_ms - now_ms).clamp(0, i64::from(u32::MAX));
        (Some(record.ring_deadline_ms), u32::try_from(left).ok())
    } else {
        (None, None)
    };
    Some(CallView {
        call_id: record.call_id,
        peer,
        direction,
        status: record.state,
        offer,
        expires_at_ms,
        expires_in_ms,
        answered_ms: if ringing { None } else { record.answered_ms },
        this_device,
    })
}

fn on_frame(
    record: &CallRecord,
    sender: UserId,
    event: CallEvent,
    now_ms: i64,
    timing: &Timing,
) -> Result<Outcome, Refusal> {
    let ringing = record.state == CallState::Ringing;
    match event {
        CallEvent::Accept { device, answer } => {
            if !ringing {
                return if sender == record.callee && record.callee_device == Some(device) {
                    // A retry of the accept that answered (its reply was lost): the caller
                    // gets `CallAccepted` again; nothing else changes.
                    Ok(Outcome {
                        record: Some(record.clone()),
                        deliveries: vec![Delivery::Accepted {
                            to: record.caller_device,
                            answer,
                            device,
                        }],
                        ended: None,
                        answered: false,
                        changed: false,
                    })
                } else if sender == record.callee {
                    Err(Refusal::AlreadyAnswered)
                } else {
                    Err(Refusal::NotFound)
                };
            }
            if !is_ringing_device(record, sender, device) {
                return Err(Refusal::NotFound);
            }
            if now_ms >= record.ring_deadline_ms {
                return Ok(end(record, EndReason::Missed, now_ms));
            }
            Ok(accept(record, device, answer, now_ms, timing))
        }
        CallEvent::Decline { device, busy } => {
            if ringing && is_ringing_device(record, sender, device) {
                let reason = if busy {
                    EndReason::Busy
                } else {
                    EndReason::Declined
                };
                Ok(end(record, reason, now_ms))
            } else {
                Err(Refusal::NotFound)
            }
        }
        CallEvent::Cancel { device } => {
            if !is_caller_device(record, sender, device) {
                Err(Refusal::NotFound)
            } else if ringing {
                Ok(end(record, EndReason::Cancelled, now_ms))
            } else {
                Err(Refusal::AlreadyAnswered)
            }
        }
        CallEvent::Hangup { device, failed } => {
            if ringing {
                if is_caller_device(record, sender, device) {
                    Ok(end(record, EndReason::Cancelled, now_ms))
                } else if is_ringing_device(record, sender, device) {
                    Ok(end(record, EndReason::Declined, now_ms))
                } else {
                    Err(Refusal::NotFound)
                }
            } else if party_side(record, sender, device).is_some() {
                let reason = if failed {
                    EndReason::Failed
                } else {
                    EndReason::Completed
                };
                Ok(end(record, reason, now_ms))
            } else {
                Err(Refusal::NotFound)
            }
        }
        CallEvent::Ice { device, candidate } => {
            if ringing {
                if !is_caller_device(record, sender, device) {
                    return Err(Refusal::NotFound);
                }
                if record.ice_caller >= MAX_ICE_PER_SIDE
                    || record.pending_ice.len() >= MAX_PENDING_ICE
                {
                    return Err(Refusal::Limit);
                }
                let mut next = record.clone();
                next.ice_caller += 1;
                next.pending_ice.push(candidate);
                return Ok(continues(next, Vec::new()));
            }
            let side = party_side(record, sender, device).ok_or(Refusal::NotFound)?;
            let mut next = record.clone();
            let sent = match side {
                Side::Caller => &mut next.ice_caller,
                Side::Callee => &mut next.ice_callee,
            };
            if *sent >= MAX_ICE_PER_SIDE {
                return Err(Refusal::Limit);
            }
            *sent += 1;
            let (user, to) = peer_of(record, side).ok_or(Refusal::NotFound)?;
            Ok(continues(
                next,
                vec![Delivery::Ice {
                    user,
                    to,
                    candidate,
                }],
            ))
        }
        CallEvent::Renegotiate { device, sdp, offer } => {
            if ringing {
                return Err(Refusal::NotFound);
            }
            let side = party_side(record, sender, device).ok_or(Refusal::NotFound)?;
            let mut next = record.clone();
            let sent = match side {
                Side::Caller => &mut next.reneg_caller,
                Side::Callee => &mut next.reneg_callee,
            };
            if *sent >= MAX_RENEGOTIATIONS {
                return Err(Refusal::Limit);
            }
            *sent += 1;
            let (user, to) = peer_of(record, side).ok_or(Refusal::NotFound)?;
            Ok(continues(
                next,
                vec![Delivery::Renegotiate {
                    user,
                    to,
                    sdp,
                    offer,
                }],
            ))
        }
        CallEvent::Attach { .. } | CallEvent::Detach { .. } | CallEvent::Tick { .. } => {
            Err(Refusal::NotFound)
        }
    }
}

fn accept(
    record: &CallRecord,
    device: DeviceId,
    answer: String,
    now_ms: i64,
    timing: &Timing,
) -> Outcome {
    let mut next = record.clone();
    let others: Vec<DeviceId> = next
        .ringing_devices
        .iter()
        .copied()
        .filter(|ringing| *ringing != device)
        .collect();
    let mut deliveries = vec![Delivery::Accepted {
        to: record.caller_device,
        answer,
        device,
    }];
    if !others.is_empty() {
        deliveries.push(Delivery::Taken { to: others });
    }
    deliveries.extend(
        std::mem::take(&mut next.pending_ice)
            .into_iter()
            .map(|candidate| Delivery::Ice {
                user: record.callee,
                to: device,
                candidate,
            }),
    );
    next.state = CallState::Active;
    next.callee_device = Some(device);
    next.answered_ms = Some(now_ms);
    next.offer = None;
    next.ringing_devices = Vec::new();
    next.callee_away_ms = None;
    next.due_ms = active_due(&next, now_ms, timing);
    Outcome {
        record: Some(next),
        deliveries,
        ended: None,
        answered: true,
        changed: true,
    }
}

fn attach(
    record: &CallRecord,
    user: UserId,
    device: DeviceId,
    now_ms: i64,
    timing: &Timing,
) -> Outcome {
    let mut next = record.clone();
    match record.state {
        CallState::Ringing => {
            if is_caller_device(record, user, device) {
                if next.caller_away_ms.take().is_none() {
                    return unchanged(record);
                }
                next.due_ms = ringing_due(&next, timing);
            } else if user == record.callee && !record.ringing_devices.contains(&device) {
                next.ringing_devices.push(device);
            } else {
                return unchanged(record);
            }
        }
        CallState::Active => {
            let Some(side) = party_side(record, user, device) else {
                return unchanged(record);
            };
            if away_mut(&mut next, side).take().is_none() {
                return unchanged(record);
            }
            next.due_ms = active_due(&next, now_ms, timing);
        }
    }
    continues(next, Vec::new())
}

fn detach(
    record: &CallRecord,
    user: UserId,
    device: DeviceId,
    now_ms: i64,
    timing: &Timing,
) -> Outcome {
    let mut next = record.clone();
    match record.state {
        CallState::Ringing => {
            if is_caller_device(record, user, device) {
                if next.caller_away_ms.is_some() {
                    return unchanged(record);
                }
                next.caller_away_ms = Some(now_ms);
                next.due_ms = ringing_due(&next, timing);
            } else if is_ringing_device(record, user, device) {
                next.ringing_devices.retain(|ringing| *ringing != device);
            } else {
                return unchanged(record);
            }
        }
        CallState::Active => {
            let Some(side) = party_side(record, user, device) else {
                return unchanged(record);
            };
            let away = away_mut(&mut next, side);
            if away.is_some() {
                return unchanged(record);
            }
            *away = Some(now_ms);
            next.due_ms = active_due(&next, now_ms, timing);
        }
    }
    continues(next, Vec::new())
}

fn tick(
    record: &CallRecord,
    gone: &[Side],
    live: &[Side],
    now_ms: i64,
    timing: &Timing,
) -> Outcome {
    let mut next = record.clone();
    match record.state {
        CallState::Ringing => {
            if now_ms >= record.ring_deadline_ms {
                return end(record, EndReason::Missed, now_ms);
            }
            if grace_over(record.caller_away_ms, now_ms, timing) {
                return end(record, EndReason::Failed, now_ms);
            }
            next.due_ms = ringing_due(&next, timing);
        }
        CallState::Active => {
            // A side found connected is back, even if no attach cleared its away time
            // (an away time written by a tick racing the attach).
            for side in live {
                *away_mut(&mut next, *side) = None;
            }
            if grace_over(next.caller_away_ms, now_ms, timing)
                || grace_over(next.callee_away_ms, now_ms, timing)
            {
                return end(record, EndReason::Failed, now_ms);
            }
            for side in gone {
                let away = away_mut(&mut next, *side);
                if away.is_none() {
                    *away = Some(now_ms);
                }
            }
            next.due_ms = active_due(&next, now_ms, timing);
        }
    }
    continues(next, Vec::new())
}

/// Ends the call: `CallEnded` to every involved device (spec D9), the caller's first.
fn end(record: &CallRecord, reason: EndReason, now_ms: i64) -> Outcome {
    let duration_s = record
        .answered_ms
        .map(|answered| u32::try_from((now_ms - answered).max(0) / 1000).unwrap_or(u32::MAX));
    let callee_devices = match record.state {
        CallState::Ringing => record.ringing_devices.clone(),
        CallState::Active => record.callee_device.into_iter().collect(),
    };
    let mut deliveries = vec![Delivery::Ended {
        user: record.caller,
        to: vec![record.caller_device],
        reason,
        duration_s,
    }];
    if !callee_devices.is_empty() {
        deliveries.push(Delivery::Ended {
            user: record.callee,
            to: callee_devices,
            reason,
            duration_s,
        });
    }
    Outcome {
        record: None,
        deliveries,
        ended: Some(reason),
        answered: false,
        changed: true,
    }
}

fn continues(next: CallRecord, deliveries: Vec<Delivery>) -> Outcome {
    Outcome {
        record: Some(next),
        deliveries,
        ended: None,
        answered: false,
        changed: true,
    }
}

fn unchanged(record: &CallRecord) -> Outcome {
    Outcome {
        record: Some(record.clone()),
        deliveries: Vec::new(),
        ended: None,
        answered: false,
        changed: false,
    }
}

fn is_caller_device(record: &CallRecord, user: UserId, device: DeviceId) -> bool {
    user == record.caller && device == record.caller_device
}

fn is_ringing_device(record: &CallRecord, user: UserId, device: DeviceId) -> bool {
    record.state == CallState::Ringing
        && user == record.callee
        && record.ringing_devices.contains(&device)
}

/// The side whose party device `device` of `user` is, in an active call.
fn party_side(record: &CallRecord, user: UserId, device: DeviceId) -> Option<Side> {
    if record.state != CallState::Active {
        None
    } else if is_caller_device(record, user, device) {
        Some(Side::Caller)
    } else if user == record.callee && record.callee_device == Some(device) {
        Some(Side::Callee)
    } else {
        None
    }
}

/// The other party's user and device, in an active call.
fn peer_of(record: &CallRecord, side: Side) -> Option<(UserId, DeviceId)> {
    match side {
        Side::Caller => record.callee_device.map(|device| (record.callee, device)),
        Side::Callee => Some((record.caller, record.caller_device)),
    }
}

fn away_mut(record: &mut CallRecord, side: Side) -> &mut Option<i64> {
    match side {
        Side::Caller => &mut record.caller_away_ms,
        Side::Callee => &mut record.callee_away_ms,
    }
}

fn grace_over(away_ms: Option<i64>, now_ms: i64, timing: &Timing) -> bool {
    away_ms.is_some_and(|away| now_ms >= away + timing.grace_ms)
}

/// Ringing: the ring deadline, or the caller's grace end if earlier.
fn ringing_due(record: &CallRecord, timing: &Timing) -> i64 {
    record
        .caller_away_ms
        .map_or(record.ring_deadline_ms, |away| {
            record.ring_deadline_ms.min(away + timing.grace_ms)
        })
}

/// Active: the next liveness check, or the earliest grace end if earlier. Ticks stay at
/// most one liveness period apart, which keeps refreshing the record's expiry.
fn active_due(record: &CallRecord, now_ms: i64, timing: &Timing) -> i64 {
    [record.caller_away_ms, record.callee_away_ms]
        .into_iter()
        .flatten()
        .map(|away| away + timing.grace_ms)
        .fold(now_ms + timing.liveness_ms, i64::min)
}
