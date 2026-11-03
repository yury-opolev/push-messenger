//! Between call frames (`push.v1`) and the call domain: checked inputs in, `CallSignal`
//! notifications and `CallState`s out.

use prost::Message;
use push_domain::calls::{
    CallDirection, CallEvent, CallInputError, CallRecord, CallState, CallView, Candidate,
    CandidateInput, Delivery, EndReason, check_candidate, check_sdp,
};
use push_domain::{DeviceId, UserId};
use push_proto::internal::v1::{
    CallSignal, Notification, call_request, call_signal, notification::Kind,
};
use push_proto::v1::{self, CallEndReason, CallMedia, CallSdpType};
use push_proto::{id_bytes, notify_channel};
use tonic::Status;
use uuid::Uuid;

use crate::convert::{invalid, parse_id};
use crate::metrics::SignalFrame;

/// A checked `CallInvite`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Invite {
    pub call_id: Uuid,
    pub callee: UserId,
    pub offer: String,
}

/// Checks a `CallInvite`: a UUIDv7 call id, a callee id, exactly audio, an audio-only offer.
pub(crate) fn invite(frame: v1::CallInvite) -> Result<Invite, Status> {
    let call_id = call_id(&frame.call_id)?;
    if call_id.get_version_num() != 7 {
        return Err(invalid("call_id: must be a UUIDv7"));
    }
    let callee = parse_id(&frame.callee_user_id, "callee_user_id", UserId)?;
    if frame.media.contains(&(CallMedia::Video as i32)) {
        return Err(invalid("video is not available"));
    }
    if frame.media != [CallMedia::Audio as i32] {
        return Err(invalid("media: must be exactly [CALL_MEDIA_AUDIO]"));
    }
    sdp("offer_sdp", &frame.offer_sdp)?;
    Ok(Invite {
        call_id,
        callee,
        offer: frame.offer_sdp,
    })
}

/// Checks a call frame other than `CallInvite` from `device`: its call id and the event.
pub(crate) fn frame_event(
    device: DeviceId,
    frame: call_request::Frame,
) -> Result<(Uuid, CallEvent), Status> {
    match frame {
        call_request::Frame::Invite(_) => Err(invalid("an invite is not a frame of a call")),
        call_request::Frame::Accept(accept) => {
            sdp("answer_sdp", &accept.answer_sdp)?;
            let event = CallEvent::Accept {
                device,
                answer: accept.answer_sdp,
            };
            Ok((call_id(&accept.call_id)?, event))
        }
        call_request::Frame::Decline(decline) => {
            let busy = match decline.reason.map(CallEndReason::try_from) {
                None | Some(Ok(CallEndReason::Declined)) => false,
                Some(Ok(CallEndReason::Busy)) => true,
                Some(_) => return Err(invalid("reason: must be DECLINED or BUSY")),
            };
            Ok((
                call_id(&decline.call_id)?,
                CallEvent::Decline { device, busy },
            ))
        }
        call_request::Frame::Cancel(cancel) => {
            Ok((call_id(&cancel.call_id)?, CallEvent::Cancel { device }))
        }
        call_request::Frame::Ice(ice) => {
            check_candidate(&CandidateInput {
                candidate: &ice.candidate,
                sdp_mid: ice.sdp_mid.as_deref(),
                username_fragment: ice.username_fragment.as_deref(),
                end_of_candidates: ice.end_of_candidates,
            })
            .map_err(|error| invalid(error.to_string()))?;
            let call = call_id(&ice.call_id)?;
            let candidate = Candidate {
                candidate: ice.candidate,
                sdp_mid: ice.sdp_mid,
                sdp_mline_index: ice.sdp_mline_index,
                end_of_candidates: ice.end_of_candidates,
                username_fragment: ice.username_fragment,
            };
            Ok((call, CallEvent::Ice { device, candidate }))
        }
        call_request::Frame::Hangup(hangup) => {
            let failed = match hangup.reason.map(CallEndReason::try_from) {
                None | Some(Ok(CallEndReason::Completed)) => false,
                Some(Ok(CallEndReason::Failed)) => true,
                Some(_) => return Err(invalid("reason: must be COMPLETED or FAILED")),
            };
            Ok((
                call_id(&hangup.call_id)?,
                CallEvent::Hangup { device, failed },
            ))
        }
        call_request::Frame::Renegotiate(renegotiate) => {
            let offer = match CallSdpType::try_from(renegotiate.r#type) {
                Ok(CallSdpType::Offer) => true,
                Ok(CallSdpType::Answer) => false,
                _ => return Err(invalid("type: must be OFFER or ANSWER")),
            };
            sdp("sdp", &renegotiate.sdp)?;
            let event = CallEvent::Renegotiate {
                device,
                sdp: renegotiate.sdp,
                offer,
            };
            Ok((call_id(&renegotiate.call_id)?, event))
        }
    }
}

/// The metric label of a frame.
pub(crate) fn signal_frame(frame: &call_request::Frame) -> SignalFrame {
    match frame {
        call_request::Frame::Invite(_) => SignalFrame::Invite,
        call_request::Frame::Accept(_) => SignalFrame::Accept,
        call_request::Frame::Decline(_) => SignalFrame::Decline,
        call_request::Frame::Cancel(_) => SignalFrame::Cancel,
        call_request::Frame::Ice(_) => SignalFrame::Ice,
        call_request::Frame::Hangup(_) => SignalFrame::Hangup,
        call_request::Frame::Renegotiate(_) => SignalFrame::Renegotiate,
    }
}

/// The call id of a frame.
pub(crate) fn call_id(bytes: &[u8]) -> Result<Uuid, Status> {
    parse_id(bytes, "call_id", std::convert::identity)
}

fn sdp(field: &str, sdp: &str) -> Result<(), Status> {
    check_sdp(sdp).map_err(|error| match error {
        CallInputError::Video => invalid("video is not available"),
        other => invalid(format!("{field}: {other}")),
    })
}

/// A reason on the wire.
pub(crate) fn end_reason(reason: EndReason) -> CallEndReason {
    match reason {
        EndReason::Completed => CallEndReason::Completed,
        EndReason::Missed => CallEndReason::Missed,
        EndReason::Declined => CallEndReason::Declined,
        EndReason::Cancelled => CallEndReason::Cancelled,
        EndReason::Busy => CallEndReason::Busy,
        EndReason::Failed => CallEndReason::Failed,
        EndReason::Unreachable => CallEndReason::Unreachable,
    }
}

/// A `CallEnded`.
pub(crate) fn ended(call: Uuid, reason: EndReason, duration_s: Option<u32>) -> v1::CallEnded {
    v1::CallEnded {
        call_id: id_bytes(call),
        reason: end_reason(reason).into(),
        duration_s,
    }
}

/// The notification channel and encoded `Notification` of `delivery`, a delivery of the
/// call `record` (the record before the event, or the new one for an invite) at `now_ms`.
pub(crate) fn signal(record: &CallRecord, delivery: Delivery, now_ms: i64) -> (String, Vec<u8>) {
    let call_id = id_bytes(record.call_id);
    let (user, to, frame) = match delivery {
        Delivery::Incoming { to } => {
            let left = (record.ring_deadline_ms - now_ms).clamp(0, i64::from(u32::MAX));
            let incoming = v1::CallIncoming {
                call_id,
                caller_user_id: id_bytes(record.caller.0),
                offer_sdp: record.offer.clone().unwrap_or_default(),
                media: vec![CallMedia::Audio.into()],
                expires_at: record.ring_deadline_ms * 1000,
                expires_in_ms: u32::try_from(left).unwrap_or(u32::MAX),
            };
            (record.callee, to, call_signal::Frame::Incoming(incoming))
        }
        Delivery::Accepted { to, answer, device } => {
            let accepted = v1::CallAccepted {
                call_id,
                answer_sdp: answer,
                answered_device_id: id_bytes(device.0),
            };
            (
                record.caller,
                vec![to],
                call_signal::Frame::Accepted(accepted),
            )
        }
        Delivery::Taken { to } => (
            record.callee,
            to,
            call_signal::Frame::Taken(v1::CallTaken { call_id }),
        ),
        Delivery::Ended {
            user,
            to,
            reason,
            duration_s,
        } => (
            user,
            to,
            call_signal::Frame::Ended(ended(record.call_id, reason, duration_s)),
        ),
        Delivery::Ice {
            user,
            to,
            candidate,
        } => {
            let ice = v1::CallIce {
                call_id,
                candidate: candidate.candidate,
                sdp_mid: candidate.sdp_mid,
                sdp_mline_index: candidate.sdp_mline_index,
                end_of_candidates: candidate.end_of_candidates,
                username_fragment: candidate.username_fragment,
            };
            (user, vec![to], call_signal::Frame::Ice(ice))
        }
        Delivery::Renegotiate {
            user,
            to,
            sdp,
            offer,
        } => {
            let kind = if offer {
                CallSdpType::Offer
            } else {
                CallSdpType::Answer
            };
            let renegotiate = v1::CallRenegotiate {
                call_id,
                sdp,
                r#type: kind.into(),
            };
            (user, vec![to], call_signal::Frame::Renegotiate(renegotiate))
        }
    };
    let notification = Notification {
        kind: Some(Kind::Call(CallSignal {
            device_ids: to.into_iter().map(|device| id_bytes(device.0)).collect(),
            frame: Some(frame),
        })),
    };
    (notify_channel(user.0), notification.encode_to_vec())
}

/// The `CallHistoryChanged` notification for `devices` of `user`: a call ended at
/// `latest_ended_ms` (Redis-clock milliseconds), or `None` when a call was hidden.
pub(crate) fn history_changed(
    user: UserId,
    devices: &[DeviceId],
    latest_ended_ms: Option<i64>,
) -> (String, Vec<u8>) {
    let notification = Notification {
        kind: Some(Kind::Call(CallSignal {
            device_ids: devices.iter().map(|device| id_bytes(device.0)).collect(),
            frame: Some(call_signal::Frame::HistoryChanged(v1::CallHistoryChanged {
                latest_ended_at: latest_ended_ms.map(|at| at * 1000),
            })),
        })),
    };
    (notify_channel(user.0), notification.encode_to_vec())
}

/// A call direction on the wire.
pub(crate) fn direction(direction: CallDirection) -> v1::CallDirection {
    match direction {
        CallDirection::In => v1::CallDirection::In,
        CallDirection::Out => v1::CallDirection::Out,
    }
}

/// A call as `CallSync` lists it.
pub(crate) fn call_state(view: &CallView) -> v1::CallState {
    let direction = direction(view.direction);
    let status = match view.status {
        CallState::Ringing => v1::CallStatus::Ringing,
        CallState::Active => v1::CallStatus::Active,
    };
    v1::CallState {
        call_id: id_bytes(view.call_id),
        peer_user_id: id_bytes(view.peer.0),
        direction: direction.into(),
        status: status.into(),
        media: vec![CallMedia::Audio.into()],
        offer_sdp: view.offer.clone(),
        expires_at: view.expires_at_ms.map(|at| at * 1000),
        expires_in_ms: view.expires_in_ms,
        answered_at: view.answered_ms.map(|at| at * 1000),
        this_device: view.this_device,
    }
}

#[cfg(test)]
mod tests {
    use push_domain::calls::{Timing, start, sync_view};
    use push_proto::error_from_status;
    use push_proto::v1::ErrorCode;

    use super::*;

    const OFFER: &str = "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 opus/48000/2\r\n";

    fn decode(message: &(String, Vec<u8>)) -> CallSignal {
        match Notification::decode(message.1.as_slice()).unwrap().kind {
            Some(Kind::Call(signal)) => signal,
            other => panic!("not a call signal: {other:?}"),
        }
    }

    fn message_of(status: &Status) -> (ErrorCode, String) {
        let error = error_from_status(status);
        (error.code(), error.message)
    }

    fn ringing() -> (CallRecord, Vec<DeviceId>) {
        let devices = vec![DeviceId::new(), DeviceId::new()];
        let (record, _) = start(
            Uuid::now_v7(),
            UserId::new(),
            DeviceId::new(),
            UserId::new(),
            OFFER.to_owned(),
            devices.clone(),
            1_000_000,
            &Timing::default(),
        );
        (record, devices)
    }

    #[test]
    fn a_delivery_becomes_a_signal_for_its_devices() {
        let (record, devices) = ringing();
        let message = signal(
            &record,
            Delivery::Incoming {
                to: devices.clone(),
            },
            1_010_000,
        );
        assert_eq!(message.0, notify_channel(record.callee.0));
        let signal_of = decode(&message);
        assert_eq!(
            signal_of.device_ids,
            devices
                .iter()
                .map(|device| id_bytes(device.0))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            signal_of.frame,
            Some(call_signal::Frame::Incoming(v1::CallIncoming {
                call_id: id_bytes(record.call_id),
                caller_user_id: id_bytes(record.caller.0),
                offer_sdp: OFFER.to_owned(),
                media: vec![CallMedia::Audio.into()],
                expires_at: 1_045_000_000,
                expires_in_ms: 35_000,
            }))
        );

        let answering = devices[0];
        let accepted = signal(
            &record,
            Delivery::Accepted {
                to: record.caller_device,
                answer: "answer".to_owned(),
                device: answering,
            },
            0,
        );
        assert_eq!(accepted.0, notify_channel(record.caller.0));
        let accepted = decode(&accepted);
        assert_eq!(accepted.device_ids, [id_bytes(record.caller_device.0)]);
        assert_eq!(
            accepted.frame,
            Some(call_signal::Frame::Accepted(v1::CallAccepted {
                call_id: id_bytes(record.call_id),
                answer_sdp: "answer".to_owned(),
                answered_device_id: id_bytes(answering.0),
            }))
        );

        let taken = signal(
            &record,
            Delivery::Taken {
                to: vec![devices[1]],
            },
            0,
        );
        assert_eq!(taken.0, notify_channel(record.callee.0));
        assert_eq!(
            decode(&taken).frame,
            Some(call_signal::Frame::Taken(v1::CallTaken {
                call_id: id_bytes(record.call_id),
            }))
        );

        let ended = signal(
            &record,
            Delivery::Ended {
                user: record.caller,
                to: vec![record.caller_device],
                reason: EndReason::Completed,
                duration_s: Some(61),
            },
            0,
        );
        assert_eq!(ended.0, notify_channel(record.caller.0));
        assert_eq!(
            decode(&ended).frame,
            Some(call_signal::Frame::Ended(v1::CallEnded {
                call_id: id_bytes(record.call_id),
                reason: CallEndReason::Completed.into(),
                duration_s: Some(61),
            }))
        );

        let renegotiate = signal(
            &record,
            Delivery::Renegotiate {
                user: record.callee,
                to: answering,
                sdp: OFFER.to_owned(),
                offer: true,
            },
            0,
        );
        assert_eq!(
            decode(&renegotiate).frame,
            Some(call_signal::Frame::Renegotiate(v1::CallRenegotiate {
                call_id: id_bytes(record.call_id),
                sdp: OFFER.to_owned(),
                r#type: CallSdpType::Offer.into(),
            }))
        );
    }

    #[test]
    fn an_ice_frame_is_relayed_as_it_came() {
        let (record, _) = ringing();
        let device = DeviceId::new();
        let frame = v1::CallIce {
            call_id: id_bytes(record.call_id),
            candidate: "candidate:1 1 udp 2122260223 192.0.2.1 54400 typ host".to_owned(),
            sdp_mid: Some("0".to_owned()),
            sdp_mline_index: Some(0),
            end_of_candidates: false,
            username_fragment: Some("abcd".to_owned()),
        };
        let (call, event) = frame_event(device, call_request::Frame::Ice(frame.clone())).unwrap();
        assert_eq!(call, record.call_id);
        let CallEvent::Ice {
            device: from,
            candidate,
        } = event
        else {
            panic!("not an ice event");
        };
        assert_eq!(from, device);
        let relayed = signal(
            &record,
            Delivery::Ice {
                user: record.callee,
                to: device,
                candidate,
            },
            0,
        );
        assert_eq!(decode(&relayed).frame, Some(call_signal::Frame::Ice(frame)));

        let bad = v1::CallIce {
            call_id: id_bytes(record.call_id),
            candidate: "not a candidate".to_owned(),
            ..v1::CallIce::default()
        };
        let status = frame_event(device, call_request::Frame::Ice(bad)).unwrap_err();
        assert_eq!(message_of(&status).0, ErrorCode::Invalid);
    }

    #[test]
    fn invites_are_checked() {
        let callee = UserId::new();
        let good = v1::CallInvite {
            call_id: id_bytes(Uuid::now_v7()),
            callee_user_id: id_bytes(callee.0),
            offer_sdp: OFFER.to_owned(),
            media: vec![CallMedia::Audio.into()],
        };
        let checked = invite(good.clone()).unwrap();
        assert_eq!(checked.callee, callee);
        assert_eq!(checked.offer, OFFER);

        let cases = [
            (
                v1::CallInvite {
                    call_id: id_bytes(Uuid::new_v4()),
                    ..good.clone()
                },
                "call_id: must be a UUIDv7",
            ),
            (
                v1::CallInvite {
                    call_id: vec![1, 2],
                    ..good.clone()
                },
                "call_id: an id must be 16 bytes, not 2",
            ),
            (
                v1::CallInvite {
                    media: vec![CallMedia::Audio.into(), CallMedia::Video.into()],
                    ..good.clone()
                },
                "video is not available",
            ),
            (
                v1::CallInvite {
                    media: Vec::new(),
                    ..good.clone()
                },
                "media: must be exactly [CALL_MEDIA_AUDIO]",
            ),
            (
                v1::CallInvite {
                    offer_sdp: format!("{OFFER}m=video 9 UDP/TLS/RTP/SAVPF 96\r\n"),
                    ..good.clone()
                },
                "video is not available",
            ),
            (
                v1::CallInvite {
                    offer_sdp: String::new(),
                    ..good
                },
                "offer_sdp: the SDP is empty",
            ),
        ];
        for (frame, expected) in cases {
            let status = invite(frame).unwrap_err();
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            assert_eq!(
                message_of(&status),
                (ErrorCode::Invalid, expected.to_owned())
            );
        }
    }

    #[test]
    fn frames_become_events_and_bad_reasons_are_invalid() {
        let device = DeviceId::new();
        let call = Uuid::now_v7();
        let cases = [
            (
                call_request::Frame::Decline(v1::CallDecline {
                    call_id: id_bytes(call),
                    reason: None,
                }),
                CallEvent::Decline {
                    device,
                    busy: false,
                },
            ),
            (
                call_request::Frame::Decline(v1::CallDecline {
                    call_id: id_bytes(call),
                    reason: Some(CallEndReason::Busy.into()),
                }),
                CallEvent::Decline { device, busy: true },
            ),
            (
                call_request::Frame::Hangup(v1::CallHangup {
                    call_id: id_bytes(call),
                    reason: Some(CallEndReason::Failed.into()),
                }),
                CallEvent::Hangup {
                    device,
                    failed: true,
                },
            ),
            (
                call_request::Frame::Cancel(v1::CallCancel {
                    call_id: id_bytes(call),
                }),
                CallEvent::Cancel { device },
            ),
            (
                call_request::Frame::Renegotiate(v1::CallRenegotiate {
                    call_id: id_bytes(call),
                    sdp: OFFER.to_owned(),
                    r#type: CallSdpType::Answer.into(),
                }),
                CallEvent::Renegotiate {
                    device,
                    sdp: OFFER.to_owned(),
                    offer: false,
                },
            ),
            (
                call_request::Frame::Accept(v1::CallAccept {
                    call_id: id_bytes(call),
                    answer_sdp: OFFER.to_owned(),
                }),
                CallEvent::Accept {
                    device,
                    answer: OFFER.to_owned(),
                },
            ),
        ];
        for (frame, expected) in cases {
            assert_eq!(frame_event(device, frame).unwrap(), (call, expected));
        }
        let refused = [
            call_request::Frame::Decline(v1::CallDecline {
                call_id: id_bytes(call),
                reason: Some(CallEndReason::Missed.into()),
            }),
            call_request::Frame::Hangup(v1::CallHangup {
                call_id: id_bytes(call),
                reason: Some(CallEndReason::Busy.into()),
            }),
            call_request::Frame::Renegotiate(v1::CallRenegotiate {
                call_id: id_bytes(call),
                sdp: OFFER.to_owned(),
                r#type: CallSdpType::Unspecified.into(),
            }),
            call_request::Frame::Accept(v1::CallAccept {
                call_id: id_bytes(call),
                answer_sdp: "nonsense".to_owned(),
            }),
            call_request::Frame::Cancel(v1::CallCancel {
                call_id: vec![0; 3],
            }),
        ];
        for frame in refused {
            let status = frame_event(device, frame).unwrap_err();
            assert_eq!(message_of(&status).0, ErrorCode::Invalid);
        }
    }

    #[test]
    fn a_view_becomes_a_call_state() {
        let (record, devices) = ringing();
        let view = sync_view(&record, record.callee, devices[0], 1_040_000).unwrap();
        assert_eq!(
            call_state(&view),
            v1::CallState {
                call_id: id_bytes(record.call_id),
                peer_user_id: id_bytes(record.caller.0),
                direction: v1::CallDirection::In.into(),
                status: v1::CallStatus::Ringing.into(),
                media: vec![CallMedia::Audio.into()],
                offer_sdp: Some(OFFER.to_owned()),
                expires_at: Some(1_045_000_000),
                expires_in_ms: Some(5_000),
                answered_at: None,
                this_device: true,
            }
        );
    }
}
