//! The call state machine: one case per row of spec §3.3, the races and the limits.

use uuid::Uuid;

use crate::calls::*;
use crate::ids::{DeviceId, UserId};

const NOW: i64 = 1_700_000_000_000;
const DEADLINE: i64 = NOW + 45_000;
const ANSWERED: i64 = NOW + 5_000;

fn timing() -> Timing {
    Timing::default()
}

fn call_id() -> Uuid {
    Uuid::from_u128(0xca11)
}
fn caller() -> UserId {
    UserId(Uuid::from_u128(0xa))
}
fn callee() -> UserId {
    UserId(Uuid::from_u128(0xb))
}
fn stranger() -> UserId {
    UserId(Uuid::from_u128(0xc))
}
fn caller_dev() -> DeviceId {
    DeviceId(Uuid::from_u128(0xa1))
}
fn d1() -> DeviceId {
    DeviceId(Uuid::from_u128(0xb1))
}
fn d2() -> DeviceId {
    DeviceId(Uuid::from_u128(0xb2))
}
/// A callee device that was not attached when the call started.
fn d3() -> DeviceId {
    DeviceId(Uuid::from_u128(0xb3))
}
fn stranger_dev() -> DeviceId {
    DeviceId(Uuid::from_u128(0xc1))
}

fn cand(n: u32) -> Candidate {
    Candidate {
        candidate: format!("candidate:{n} 1 udp 2122260223 192.0.2.1 54400 typ host"),
        sdp_mid: Some("0".into()),
        sdp_mline_index: Some(0),
        end_of_candidates: false,
        username_fragment: Some("EsAw".into()),
    }
}

/// A call ringing on `d1` and `d2` since `NOW`.
fn ringing() -> CallRecord {
    start(
        call_id(),
        caller(),
        caller_dev(),
        callee(),
        "offer".into(),
        vec![d1(), d2()],
        NOW,
        &timing(),
    )
    .0
}

/// The call answered on `d1` at `ANSWERED`.
fn active() -> CallRecord {
    let outcome = transition(
        &ringing(),
        Some(callee()),
        accept(d1()),
        ANSWERED,
        &timing(),
    )
    .unwrap();
    outcome.record.unwrap()
}

fn with(mut record: CallRecord, change: impl FnOnce(&mut CallRecord)) -> CallRecord {
    change(&mut record);
    record
}

fn accept(device: DeviceId) -> CallEvent {
    CallEvent::Accept {
        device,
        answer: "answer".into(),
    }
}

fn ice(device: DeviceId, n: u32) -> CallEvent {
    CallEvent::Ice {
        device,
        candidate: cand(n),
    }
}

fn renegotiate(device: DeviceId) -> CallEvent {
    CallEvent::Renegotiate {
        device,
        sdp: "sdp".into(),
        offer: true,
    }
}

fn ended(reason: EndReason, callee_to: Vec<DeviceId>, duration_s: Option<u32>) -> Vec<Delivery> {
    vec![
        Delivery::Ended {
            user: caller(),
            to: vec![caller_dev()],
            reason,
            duration_s,
        },
        Delivery::Ended {
            user: callee(),
            to: callee_to,
            reason,
            duration_s,
        },
    ]
}

enum Expect {
    Refused(Refusal),
    Ended {
        reason: EndReason,
        deliveries: Vec<Delivery>,
    },
    Next {
        record: Box<CallRecord>,
        deliveries: Vec<Delivery>,
        answered: bool,
    },
    Unchanged,
    /// The record stays as it is (no write), but these frames are published.
    Repeated(Vec<Delivery>),
}

struct Case {
    name: &'static str,
    record: CallRecord,
    sender: Option<UserId>,
    event: CallEvent,
    now: i64,
    expect: Expect,
}

fn next(record: CallRecord, deliveries: Vec<Delivery>) -> Expect {
    Expect::Next {
        record: Box::new(record),
        deliveries,
        answered: false,
    }
}

fn check(case: Case) {
    let got = transition(&case.record, case.sender, case.event, case.now, &timing());
    let want = match case.expect {
        Expect::Refused(refusal) => Err(refusal),
        Expect::Ended { reason, deliveries } => Ok(Outcome {
            record: None,
            deliveries,
            ended: Some(reason),
            answered: false,
            changed: true,
        }),
        Expect::Next {
            record,
            deliveries,
            answered,
        } => Ok(Outcome {
            record: Some(*record),
            deliveries,
            ended: None,
            answered,
            changed: true,
        }),
        Expect::Unchanged => Ok(Outcome {
            record: Some(case.record.clone()),
            deliveries: Vec::new(),
            ended: None,
            answered: false,
            changed: false,
        }),
        Expect::Repeated(deliveries) => Ok(Outcome {
            record: Some(case.record.clone()),
            deliveries,
            ended: None,
            answered: false,
            changed: false,
        }),
    };
    assert_eq!(got, want, "case: {}", case.name);
}

#[test]
fn every_transition_of_the_spec() {
    // `Invite` rows: the caller-in-a-call, busy and unreachable rows are the service's
    // checks against the busy locks and the device registry; `start` is the "ok" row.
    let (record, deliveries) = start(
        call_id(),
        caller(),
        caller_dev(),
        callee(),
        "offer".into(),
        vec![d1(), d2()],
        NOW,
        &timing(),
    );
    assert_eq!(record.state, CallState::Ringing);
    assert_eq!(record.v, 1);
    assert_eq!(record.ring_deadline_ms, DEADLINE);
    assert_eq!(record.due_ms, DEADLINE);
    assert_eq!(record.offer.as_deref(), Some("offer"));
    assert_eq!(
        deliveries,
        vec![Delivery::Incoming {
            to: vec![d1(), d2()]
        }]
    );

    let full_buffer = with(ringing(), |r| {
        r.ice_caller = MAX_PENDING_ICE as u32;
        r.pending_ice = (0..MAX_PENDING_ICE as u32).map(cand).collect();
    });
    let caller_away = with(ringing(), |r| {
        r.caller_away_ms = Some(NOW + 1_000);
        r.due_ms = NOW + 31_000;
    });
    let callee_away = with(active(), |r| {
        r.callee_away_ms = Some(ANSWERED + 1_000);
        r.due_ms = ANSWERED + 16_000;
    });

    let cases = vec![
        Case {
            name: "ringing: accept from a ringing device before the deadline",
            record: ringing(),
            sender: Some(callee()),
            event: accept(d1()),
            now: ANSWERED,
            expect: Expect::Next {
                record: Box::new(with(ringing(), |r| {
                    r.state = CallState::Active;
                    r.callee_device = Some(d1());
                    r.answered_ms = Some(ANSWERED);
                    r.offer = None;
                    r.ringing_devices = Vec::new();
                    r.due_ms = ANSWERED + 15_000;
                })),
                deliveries: vec![
                    Delivery::Accepted {
                        to: caller_dev(),
                        answer: "answer".into(),
                        device: d1(),
                    },
                    Delivery::Taken { to: vec![d2()] },
                ],
                answered: true,
            },
        },
        Case {
            name: "ringing: accept at the deadline is missed",
            record: ringing(),
            sender: Some(callee()),
            event: accept(d1()),
            now: DEADLINE,
            expect: Expect::Ended {
                reason: EndReason::Missed,
                deliveries: ended(EndReason::Missed, vec![d1(), d2()], None),
            },
        },
        Case {
            name: "active: accept again from the answering device (a retry)",
            record: active(),
            sender: Some(callee()),
            event: accept(d1()),
            now: ANSWERED + 1,
            expect: Expect::Repeated(vec![Delivery::Accepted {
                to: caller_dev(),
                answer: "answer".into(),
                device: d1(),
            }]),
        },
        Case {
            name: "active: accept from another callee device",
            record: active(),
            sender: Some(callee()),
            event: accept(d2()),
            now: ANSWERED + 1,
            expect: Expect::Refused(Refusal::AlreadyAnswered),
        },
        Case {
            name: "ringing: decline",
            record: ringing(),
            sender: Some(callee()),
            event: CallEvent::Decline {
                device: d2(),
                busy: false,
            },
            now: NOW + 1_000,
            expect: Expect::Ended {
                reason: EndReason::Declined,
                deliveries: ended(EndReason::Declined, vec![d1(), d2()], None),
            },
        },
        Case {
            name: "ringing: decline as busy",
            record: ringing(),
            sender: Some(callee()),
            event: CallEvent::Decline {
                device: d1(),
                busy: true,
            },
            now: NOW + 1_000,
            expect: Expect::Ended {
                reason: EndReason::Busy,
                deliveries: ended(EndReason::Busy, vec![d1(), d2()], None),
            },
        },
        Case {
            name: "ringing: cancel from the caller device",
            record: ringing(),
            sender: Some(caller()),
            event: CallEvent::Cancel {
                device: caller_dev(),
            },
            now: NOW + 1_000,
            expect: Expect::Ended {
                reason: EndReason::Cancelled,
                deliveries: ended(EndReason::Cancelled, vec![d1(), d2()], None),
            },
        },
        Case {
            name: "active: cancel from the caller device",
            record: active(),
            sender: Some(caller()),
            event: CallEvent::Cancel {
                device: caller_dev(),
            },
            now: ANSWERED + 1,
            expect: Expect::Refused(Refusal::AlreadyAnswered),
        },
        Case {
            name: "active: hangup completed from the caller",
            record: active(),
            sender: Some(caller()),
            event: CallEvent::Hangup {
                device: caller_dev(),
                failed: false,
            },
            now: ANSWERED + 61_999,
            expect: Expect::Ended {
                reason: EndReason::Completed,
                deliveries: ended(EndReason::Completed, vec![d1()], Some(61)),
            },
        },
        Case {
            name: "active: hangup failed from the callee",
            record: active(),
            sender: Some(callee()),
            event: CallEvent::Hangup {
                device: d1(),
                failed: true,
            },
            now: ANSWERED + 2_000,
            expect: Expect::Ended {
                reason: EndReason::Failed,
                deliveries: ended(EndReason::Failed, vec![d1()], Some(2)),
            },
        },
        Case {
            name: "ringing: hangup from the caller is a cancel (D10)",
            record: ringing(),
            sender: Some(caller()),
            event: CallEvent::Hangup {
                device: caller_dev(),
                failed: false,
            },
            now: NOW + 1_000,
            expect: Expect::Ended {
                reason: EndReason::Cancelled,
                deliveries: ended(EndReason::Cancelled, vec![d1(), d2()], None),
            },
        },
        Case {
            name: "ringing: hangup from a ringing device is a decline (D10)",
            record: ringing(),
            sender: Some(callee()),
            event: CallEvent::Hangup {
                device: d2(),
                failed: false,
            },
            now: NOW + 1_000,
            expect: Expect::Ended {
                reason: EndReason::Declined,
                deliveries: ended(EndReason::Declined, vec![d1(), d2()], None),
            },
        },
        Case {
            name: "ringing: caller ICE is buffered",
            record: ringing(),
            sender: Some(caller()),
            event: ice(caller_dev(), 1),
            now: NOW + 1_000,
            expect: next(
                with(ringing(), |r| {
                    r.ice_caller = 1;
                    r.pending_ice = vec![cand(1)];
                }),
                Vec::new(),
            ),
        },
        Case {
            name: "ringing: caller ICE with the buffer full",
            record: full_buffer.clone(),
            sender: Some(caller()),
            event: ice(caller_dev(), 99),
            now: NOW + 1_000,
            expect: Expect::Refused(Refusal::Limit),
        },
        Case {
            name: "active: caller ICE reaches the answering device",
            record: active(),
            sender: Some(caller()),
            event: ice(caller_dev(), 7),
            now: ANSWERED + 1,
            expect: next(
                with(active(), |r| r.ice_caller = 1),
                vec![Delivery::Ice {
                    user: callee(),
                    to: d1(),
                    candidate: cand(7),
                }],
            ),
        },
        Case {
            name: "active: callee ICE reaches the caller device",
            record: active(),
            sender: Some(callee()),
            event: ice(d1(), 8),
            now: ANSWERED + 1,
            expect: next(
                with(active(), |r| r.ice_callee = 1),
                vec![Delivery::Ice {
                    user: caller(),
                    to: caller_dev(),
                    candidate: cand(8),
                }],
            ),
        },
        Case {
            name: "any: caller ICE at 200 while ringing",
            record: with(ringing(), |r| r.ice_caller = MAX_ICE_PER_SIDE),
            sender: Some(caller()),
            event: ice(caller_dev(), 1),
            now: NOW + 1_000,
            expect: Expect::Refused(Refusal::Limit),
        },
        Case {
            name: "any: caller ICE at 200 while active",
            record: with(active(), |r| r.ice_caller = MAX_ICE_PER_SIDE),
            sender: Some(caller()),
            event: ice(caller_dev(), 1),
            now: ANSWERED + 1,
            expect: Expect::Refused(Refusal::Limit),
        },
        Case {
            name: "any: callee ICE at 200 while active",
            record: with(active(), |r| r.ice_callee = MAX_ICE_PER_SIDE),
            sender: Some(callee()),
            event: ice(d1(), 1),
            now: ANSWERED + 1,
            expect: Expect::Refused(Refusal::Limit),
        },
        Case {
            name: "active: caller renegotiation reaches the answering device",
            record: active(),
            sender: Some(caller()),
            event: renegotiate(caller_dev()),
            now: ANSWERED + 1,
            expect: next(
                with(active(), |r| r.reneg_caller = 1),
                vec![Delivery::Renegotiate {
                    user: callee(),
                    to: d1(),
                    sdp: "sdp".into(),
                    offer: true,
                }],
            ),
        },
        Case {
            name: "active: callee renegotiation reaches the caller device",
            record: active(),
            sender: Some(callee()),
            event: CallEvent::Renegotiate {
                device: d1(),
                sdp: "sdp".into(),
                offer: false,
            },
            now: ANSWERED + 1,
            expect: next(
                with(active(), |r| r.reneg_callee = 1),
                vec![Delivery::Renegotiate {
                    user: caller(),
                    to: caller_dev(),
                    sdp: "sdp".into(),
                    offer: false,
                }],
            ),
        },
        Case {
            name: "active: caller renegotiation at 20",
            record: with(active(), |r| r.reneg_caller = MAX_RENEGOTIATIONS),
            sender: Some(caller()),
            event: renegotiate(caller_dev()),
            now: ANSWERED + 1,
            expect: Expect::Refused(Refusal::Limit),
        },
        Case {
            name: "active: callee renegotiation at 20",
            record: with(active(), |r| r.reneg_callee = MAX_RENEGOTIATIONS),
            sender: Some(callee()),
            event: renegotiate(d1()),
            now: ANSWERED + 1,
            expect: Expect::Refused(Refusal::Limit),
        },
        Case {
            name: "ringing: renegotiation is not found",
            record: ringing(),
            sender: Some(caller()),
            event: renegotiate(caller_dev()),
            now: NOW + 1_000,
            expect: Expect::Refused(Refusal::NotFound),
        },
        Case {
            name: "ringing: a callee device attaches and rings",
            record: ringing(),
            sender: None,
            event: CallEvent::Attach {
                user: callee(),
                device: d3(),
            },
            now: NOW + 1_000,
            expect: next(
                with(ringing(), |r| r.ringing_devices.push(d3())),
                Vec::new(),
            ),
        },
        Case {
            name: "ringing: the away caller device attaches",
            record: caller_away.clone(),
            sender: None,
            event: CallEvent::Attach {
                user: caller(),
                device: caller_dev(),
            },
            now: NOW + 2_000,
            expect: next(
                with(caller_away.clone(), |r| {
                    r.caller_away_ms = None;
                    r.due_ms = DEADLINE;
                }),
                Vec::new(),
            ),
        },
        Case {
            name: "active: the away party device attaches",
            record: callee_away.clone(),
            sender: None,
            event: CallEvent::Attach {
                user: callee(),
                device: d1(),
            },
            now: ANSWERED + 3_000,
            expect: next(
                with(callee_away.clone(), |r| {
                    r.callee_away_ms = None;
                    r.due_ms = ANSWERED + 18_000;
                }),
                Vec::new(),
            ),
        },
        Case {
            name: "active: an attach keeps the other side's earlier grace end",
            record: with(callee_away.clone(), |r| r.caller_away_ms = Some(ANSWERED)),
            sender: None,
            event: CallEvent::Attach {
                user: caller(),
                device: caller_dev(),
            },
            now: ANSWERED + 20_000,
            expect: next(
                with(callee_away.clone(), |r| r.due_ms = ANSWERED + 31_000),
                Vec::new(),
            ),
        },
        Case {
            name: "ringing: a ringing device detaches",
            record: ringing(),
            sender: None,
            event: CallEvent::Detach {
                user: callee(),
                device: d1(),
            },
            now: NOW + 1_000,
            expect: next(
                with(ringing(), |r| r.ringing_devices = vec![d2()]),
                Vec::new(),
            ),
        },
        Case {
            name: "ringing: the caller device detaches early",
            record: ringing(),
            sender: None,
            event: CallEvent::Detach {
                user: caller(),
                device: caller_dev(),
            },
            now: NOW + 5_000,
            expect: next(
                with(ringing(), |r| {
                    r.caller_away_ms = Some(NOW + 5_000);
                    r.due_ms = NOW + 35_000;
                }),
                Vec::new(),
            ),
        },
        Case {
            name: "ringing: the caller device detaches late",
            record: ringing(),
            sender: None,
            event: CallEvent::Detach {
                user: caller(),
                device: caller_dev(),
            },
            now: NOW + 20_000,
            expect: next(
                with(ringing(), |r| r.caller_away_ms = Some(NOW + 20_000)),
                Vec::new(),
            ),
        },
        Case {
            name: "active: a party device detaches",
            record: active(),
            sender: None,
            event: CallEvent::Detach {
                user: callee(),
                device: d1(),
            },
            now: ANSWERED + 1_000,
            expect: next(
                with(active(), |r| {
                    r.callee_away_ms = Some(ANSWERED + 1_000);
                    r.due_ms = ANSWERED + 16_000;
                }),
                Vec::new(),
            ),
        },
        Case {
            name: "ringing: tick at the deadline is missed",
            record: ringing(),
            sender: None,
            event: CallEvent::Tick {
                gone: Vec::new(),
                live: Vec::new(),
            },
            now: DEADLINE,
            expect: Expect::Ended {
                reason: EndReason::Missed,
                deliveries: ended(EndReason::Missed, vec![d1(), d2()], None),
            },
        },
        Case {
            name: "ringing: tick after the caller's grace fails",
            record: caller_away.clone(),
            sender: None,
            event: CallEvent::Tick {
                gone: Vec::new(),
                live: Vec::new(),
            },
            now: NOW + 31_000,
            expect: Expect::Ended {
                reason: EndReason::Failed,
                deliveries: ended(EndReason::Failed, vec![d1(), d2()], None),
            },
        },
        Case {
            name: "ringing: an early tick reschedules",
            record: with(caller_away.clone(), |r| r.caller_away_ms = None),
            sender: None,
            event: CallEvent::Tick {
                gone: Vec::new(),
                live: Vec::new(),
            },
            now: NOW + 31_000,
            expect: next(
                with(caller_away.clone(), |r| {
                    r.caller_away_ms = None;
                    r.due_ms = DEADLINE;
                }),
                Vec::new(),
            ),
        },
        Case {
            name: "active: tick after a side's grace fails",
            record: callee_away.clone(),
            sender: None,
            event: CallEvent::Tick {
                gone: Vec::new(),
                live: Vec::new(),
            },
            now: ANSWERED + 31_000,
            expect: Expect::Ended {
                reason: EndReason::Failed,
                deliveries: ended(EndReason::Failed, vec![d1()], Some(31)),
            },
        },
        Case {
            name: "active: liveness tick marks a gone side away",
            record: active(),
            sender: None,
            event: CallEvent::Tick {
                gone: vec![Side::Callee],
                live: Vec::new(),
            },
            now: ANSWERED + 15_000,
            expect: next(
                with(active(), |r| {
                    r.callee_away_ms = Some(ANSWERED + 15_000);
                    r.due_ms = ANSWERED + 30_000;
                }),
                Vec::new(),
            ),
        },
        Case {
            name: "active: liveness tick keeps an earlier away time",
            record: callee_away.clone(),
            sender: None,
            event: CallEvent::Tick {
                gone: vec![Side::Callee],
                live: Vec::new(),
            },
            now: ANSWERED + 16_000,
            expect: next(
                with(callee_away.clone(), |r| r.due_ms = ANSWERED + 31_000),
                Vec::new(),
            ),
        },
        Case {
            name: "active: liveness tick finds an away side live: away cleared, no end",
            record: callee_away.clone(),
            sender: None,
            event: CallEvent::Tick {
                gone: Vec::new(),
                live: vec![Side::Caller, Side::Callee],
            },
            now: ANSWERED + 31_000,
            expect: next(
                with(callee_away.clone(), |r| {
                    r.callee_away_ms = None;
                    r.due_ms = ANSWERED + 46_000;
                }),
                Vec::new(),
            ),
        },
        Case {
            name: "active: liveness tick clears a live side and marks a gone one",
            record: callee_away.clone(),
            sender: None,
            event: CallEvent::Tick {
                gone: vec![Side::Caller],
                live: vec![Side::Callee],
            },
            now: ANSWERED + 16_000,
            expect: next(
                with(callee_away.clone(), |r| {
                    r.callee_away_ms = None;
                    r.caller_away_ms = Some(ANSWERED + 16_000);
                    r.due_ms = ANSWERED + 31_000;
                }),
                Vec::new(),
            ),
        },
        Case {
            name: "active: tick otherwise reschedules the liveness check",
            record: active(),
            sender: None,
            event: CallEvent::Tick {
                gone: Vec::new(),
                live: Vec::new(),
            },
            now: ANSWERED + 15_000,
            expect: next(with(active(), |r| r.due_ms = ANSWERED + 30_000), Vec::new()),
        },
        Case {
            name: "any: a stranger's frame is not found",
            record: active(),
            sender: Some(stranger()),
            event: CallEvent::Hangup {
                device: stranger_dev(),
                failed: false,
            },
            now: ANSWERED + 1,
            expect: Expect::Refused(Refusal::NotFound),
        },
        Case {
            name: "any: a frame without a sender is not found",
            record: ringing(),
            sender: None,
            event: accept(d1()),
            now: NOW + 1,
            expect: Expect::Refused(Refusal::NotFound),
        },
        Case {
            name: "unrelated device attaches",
            record: ringing(),
            sender: None,
            event: CallEvent::Attach {
                user: stranger(),
                device: stranger_dev(),
            },
            now: NOW + 1,
            expect: Expect::Unchanged,
        },
        Case {
            name: "a connected caller device attaches again",
            record: ringing(),
            sender: None,
            event: CallEvent::Attach {
                user: caller(),
                device: caller_dev(),
            },
            now: NOW + 1,
            expect: Expect::Unchanged,
        },
        Case {
            name: "a ringing device attaches again",
            record: ringing(),
            sender: None,
            event: CallEvent::Attach {
                user: callee(),
                device: d1(),
            },
            now: NOW + 1,
            expect: Expect::Unchanged,
        },
        Case {
            name: "active: a callee device that did not answer attaches",
            record: active(),
            sender: None,
            event: CallEvent::Attach {
                user: callee(),
                device: d2(),
            },
            now: ANSWERED + 1,
            expect: Expect::Unchanged,
        },
        Case {
            name: "ringing: a callee device that does not ring detaches",
            record: ringing(),
            sender: None,
            event: CallEvent::Detach {
                user: callee(),
                device: d3(),
            },
            now: NOW + 1,
            expect: Expect::Unchanged,
        },
        Case {
            name: "active: an away party device detaches again",
            record: callee_away.clone(),
            sender: None,
            event: CallEvent::Detach {
                user: callee(),
                device: d1(),
            },
            now: ANSWERED + 2_000,
            expect: Expect::Unchanged,
        },
    ];
    for case in cases {
        check(case);
    }
}

fn ok(result: Result<Outcome, Refusal>) -> Outcome {
    result.expect("the event is accepted")
}

#[test]
fn two_devices_accepting_at_once() {
    let first = ok(transition(
        &ringing(),
        Some(callee()),
        accept(d1()),
        ANSWERED,
        &timing(),
    ));
    let record = first.record.unwrap();
    assert_eq!(
        transition(&record, Some(callee()), accept(d2()), ANSWERED, &timing()),
        Err(Refusal::AlreadyAnswered)
    );
}

#[test]
fn accepting_twice_from_the_same_device_repeats_the_accepted_frame() {
    let first = ok(transition(
        &ringing(),
        Some(callee()),
        accept(d1()),
        ANSWERED,
        &timing(),
    ));
    let record = first.record.unwrap();
    let again = ok(transition(
        &record,
        Some(callee()),
        accept(d1()),
        ANSWERED + 500,
        &timing(),
    ));
    assert!(!again.changed);
    assert!(!again.answered);
    assert_eq!(again.ended, None);
    assert_eq!(again.record, Some(record.clone()));
    assert_eq!(
        again.deliveries,
        vec![Delivery::Accepted {
            to: caller_dev(),
            answer: "answer".into(),
            device: d1(),
        }]
    );
    // Only the answering device may repeat it; the caller may not.
    assert_eq!(
        transition(&record, Some(caller()), accept(d1()), ANSWERED, &timing()),
        Err(Refusal::NotFound)
    );
}

#[test]
fn a_tick_racing_an_attach_never_fails_the_device() {
    // The tick read the registry before the device reattached and marked it away.
    let marked = ok(transition(
        &active(),
        None,
        CallEvent::Tick {
            gone: vec![Side::Callee],
            live: vec![Side::Caller],
        },
        ANSWERED + 15_000,
        &timing(),
    ))
    .record
    .unwrap();
    assert_eq!(marked.callee_away_ms, Some(ANSWERED + 15_000));
    // The next ticks find it connected: its away time goes, the grace end passes.
    let tick = ok(transition(
        &marked,
        None,
        CallEvent::Tick {
            gone: Vec::new(),
            live: vec![Side::Caller, Side::Callee],
        },
        ANSWERED + 45_000,
        &timing(),
    ));
    assert_eq!(tick.ended, None);
    let record = tick.record.unwrap();
    assert_eq!(record.callee_away_ms, None);
    assert_eq!(record.due_ms, ANSWERED + 60_000);
}

#[test]
fn cancel_racing_accept() {
    let cancel = CallEvent::Cancel {
        device: caller_dev(),
    };
    // The cancel commits first: the call is gone, so the accept finds no record
    // (`CALL_NOT_FOUND` from the service).
    let cancelled = ok(transition(
        &ringing(),
        Some(caller()),
        cancel.clone(),
        ANSWERED,
        &timing(),
    ));
    assert_eq!(cancelled.ended, Some(EndReason::Cancelled));
    assert_eq!(cancelled.record, None);
    // The accept commits first: the cancel is answered `ALREADY_ANSWERED`.
    let accepted = ok(transition(
        &ringing(),
        Some(callee()),
        accept(d1()),
        ANSWERED,
        &timing(),
    ));
    assert_eq!(
        transition(
            &accepted.record.unwrap(),
            Some(caller()),
            cancel,
            ANSWERED,
            &timing()
        ),
        Err(Refusal::AlreadyAnswered)
    );
}

#[test]
fn hangup_racing_reconnect() {
    let hangup = CallEvent::Hangup {
        device: d1(),
        failed: false,
    };
    let detached = ok(transition(
        &active(),
        None,
        CallEvent::Detach {
            user: callee(),
            device: d1(),
        },
        ANSWERED + 1_000,
        &timing(),
    ))
    .record
    .unwrap();
    // The hangup commits first: the call ends; a later attach finds no record.
    let ended = ok(transition(
        &detached,
        Some(callee()),
        hangup.clone(),
        ANSWERED + 2_000,
        &timing(),
    ));
    assert_eq!(ended.ended, Some(EndReason::Completed));
    assert_eq!(ended.record, None);
    // The reconnect commits first: the hangup still ends the call.
    let resumed = ok(transition(
        &detached,
        None,
        CallEvent::Attach {
            user: callee(),
            device: d1(),
        },
        ANSWERED + 2_000,
        &timing(),
    ))
    .record
    .unwrap();
    assert_eq!(resumed.callee_away_ms, None);
    let ended = ok(transition(
        &resumed,
        Some(callee()),
        hangup,
        ANSWERED + 3_000,
        &timing(),
    ));
    assert_eq!(
        ended.deliveries,
        ended_deliveries(EndReason::Completed, Some(3))
    );
}

fn ended_deliveries(reason: EndReason, duration_s: Option<u32>) -> Vec<Delivery> {
    ended(reason, vec![d1()], duration_s)
}

#[test]
fn tick_after_attach_only_reschedules() {
    let detach = ok(transition(
        &active(),
        None,
        CallEvent::Detach {
            user: caller(),
            device: caller_dev(),
        },
        ANSWERED + 1_000,
        &timing(),
    ))
    .record
    .unwrap();
    assert_eq!(detach.due_ms, ANSWERED + 16_000);
    let attach = ok(transition(
        &detach,
        None,
        CallEvent::Attach {
            user: caller(),
            device: caller_dev(),
        },
        ANSWERED + 20_000,
        &timing(),
    ))
    .record
    .unwrap();
    // The grace end the tick was claimed for has passed, but the attach cleared it.
    let tick = ok(transition(
        &attach,
        None,
        CallEvent::Tick {
            gone: Vec::new(),
            live: Vec::new(),
        },
        ANSWERED + 31_000,
        &timing(),
    ));
    assert_eq!(tick.ended, None);
    assert!(tick.changed);
    let record = tick.record.unwrap();
    assert_eq!(record.state, CallState::Active);
    assert_eq!(record.caller_away_ms, None);
    assert_eq!(record.due_ms, ANSWERED + 46_000);
}

#[test]
fn wrong_role_is_not_found() {
    let cases: Vec<(&str, CallRecord, UserId, CallEvent)> = vec![
        (
            "the caller accepts",
            ringing(),
            caller(),
            accept(caller_dev()),
        ),
        (
            "the caller accepts on a callee device id",
            ringing(),
            caller(),
            accept(d1()),
        ),
        (
            "another user's device accepts",
            ringing(),
            stranger(),
            accept(stranger_dev()),
        ),
        (
            "another user names a ringing device",
            ringing(),
            stranger(),
            accept(d1()),
        ),
        (
            "a callee device that does not ring accepts",
            ringing(),
            callee(),
            accept(d3()),
        ),
        (
            "a callee device that does not ring declines",
            ringing(),
            callee(),
            CallEvent::Decline {
                device: d3(),
                busy: false,
            },
        ),
        (
            "the callee cancels",
            ringing(),
            callee(),
            CallEvent::Cancel { device: d1() },
        ),
        (
            "the caller cancels from another device",
            ringing(),
            caller(),
            CallEvent::Cancel { device: d3() },
        ),
        (
            "the callee trickles ICE while ringing",
            ringing(),
            callee(),
            ice(d1(), 1),
        ),
        (
            "the caller renegotiates while ringing",
            ringing(),
            caller(),
            renegotiate(caller_dev()),
        ),
        (
            "the callee renegotiates while ringing",
            ringing(),
            callee(),
            renegotiate(d1()),
        ),
        (
            "the caller declines",
            ringing(),
            caller(),
            CallEvent::Decline {
                device: caller_dev(),
                busy: false,
            },
        ),
        (
            "a callee device that did not answer hangs up",
            active(),
            callee(),
            CallEvent::Hangup {
                device: d2(),
                failed: false,
            },
        ),
        (
            "a callee device that did not answer trickles ICE",
            active(),
            callee(),
            ice(d2(), 1),
        ),
        (
            "the answering device declines once active",
            active(),
            callee(),
            CallEvent::Decline {
                device: d1(),
                busy: false,
            },
        ),
        (
            "another user accepts an active call",
            active(),
            stranger(),
            accept(d1()),
        ),
        (
            "another user renegotiates",
            active(),
            stranger(),
            renegotiate(caller_dev()),
        ),
    ];
    for (name, record, sender, event) in cases {
        assert_eq!(
            transition(&record, Some(sender), event, NOW + 6_000, &timing()),
            Err(Refusal::NotFound),
            "{name}"
        );
    }
}

#[test]
fn late_accept_is_missed() {
    let late = ok(transition(
        &ringing(),
        Some(callee()),
        accept(d2()),
        DEADLINE + 1,
        &timing(),
    ));
    assert_eq!(late.ended, Some(EndReason::Missed));
    assert!(!late.answered);
    assert_eq!(late.record, None);
    // The late accepter is told as well.
    assert_eq!(
        late.deliveries,
        ended(EndReason::Missed, vec![d1(), d2()], None)
    );
}

#[test]
fn duration_is_whole_seconds_from_answer() {
    for (elapsed, seconds) in [
        (0, 0),
        (999, 0),
        (1_000, 1),
        (59_999, 59),
        (3_600_000, 3_600),
    ] {
        let outcome = ok(transition(
            &active(),
            Some(caller()),
            CallEvent::Hangup {
                device: caller_dev(),
                failed: false,
            },
            ANSWERED + elapsed,
            &timing(),
        ));
        assert_eq!(
            outcome.deliveries,
            ended(EndReason::Completed, vec![d1()], Some(seconds)),
            "{elapsed} ms"
        );
    }
}

#[test]
fn buffered_ice_reaches_the_answerer_in_order() {
    let mut record = ringing();
    for n in 1..=3 {
        let outcome = ok(transition(
            &record,
            Some(caller()),
            ice(caller_dev(), n),
            NOW + i64::from(n),
            &timing(),
        ));
        // Ringing devices never get the caller's candidates.
        assert!(outcome.deliveries.is_empty());
        record = outcome.record.unwrap();
    }
    let accepted = ok(transition(
        &record,
        Some(callee()),
        accept(d2()),
        ANSWERED,
        &timing(),
    ));
    assert_eq!(
        accepted.deliveries,
        vec![
            Delivery::Accepted {
                to: caller_dev(),
                answer: "answer".into(),
                device: d2(),
            },
            Delivery::Taken { to: vec![d1()] },
            Delivery::Ice {
                user: callee(),
                to: d2(),
                candidate: cand(1),
            },
            Delivery::Ice {
                user: callee(),
                to: d2(),
                candidate: cand(2),
            },
            Delivery::Ice {
                user: callee(),
                to: d2(),
                candidate: cand(3),
            },
        ]
    );
    let record = accepted.record.unwrap();
    assert!(record.pending_ice.is_empty());
    assert_eq!(record.ice_caller, 3);
}

#[test]
fn ice_cap_and_buffer_cap() {
    let mut record = ringing();
    for n in 0..MAX_PENDING_ICE as u32 {
        record = ok(transition(
            &record,
            Some(caller()),
            ice(caller_dev(), n),
            NOW + 1,
            &timing(),
        ))
        .record
        .unwrap();
    }
    assert_eq!(
        transition(
            &record,
            Some(caller()),
            ice(caller_dev(), 99),
            NOW + 1,
            &timing()
        ),
        Err(Refusal::Limit)
    );
    record = ok(transition(
        &record,
        Some(callee()),
        accept(d1()),
        ANSWERED,
        &timing(),
    ))
    .record
    .unwrap();
    // The buffered candidates count toward the caller's 200.
    for (sender, device, already) in [
        (caller(), caller_dev(), MAX_PENDING_ICE as u32),
        (callee(), d1(), 0),
    ] {
        for n in already..MAX_ICE_PER_SIDE {
            record = ok(transition(
                &record,
                Some(sender),
                ice(device, n),
                ANSWERED + 1,
                &timing(),
            ))
            .record
            .unwrap();
        }
        assert_eq!(
            transition(
                &record,
                Some(sender),
                ice(device, 0),
                ANSWERED + 1,
                &timing()
            ),
            Err(Refusal::Limit)
        );
    }
    assert_eq!(record.ice_caller, MAX_ICE_PER_SIDE);
    assert_eq!(record.ice_callee, MAX_ICE_PER_SIDE);
}

#[test]
fn records_are_bounded() {
    // Worst case for the JSON: inputs the validators accept made of characters JSON escapes
    // to two bytes (`\` and `"`).
    let head = "v=0\nm=audio 9 UDP/TLS/RTP/SAVPF 111\na=";
    let offer = format!("{head}{}", "\\".repeat(MAX_SDP_BYTES - head.len()));
    assert_eq!(offer.len(), MAX_SDP_BYTES);
    assert_eq!(check_sdp(&offer), Ok(()));
    let candidate = Candidate {
        candidate: format!(
            "candidate:{}",
            "\"".repeat(MAX_CANDIDATE_BYTES - "candidate:".len())
        ),
        sdp_mid: Some("\\".repeat(MAX_SDP_MID_BYTES)),
        sdp_mline_index: Some(u32::MAX),
        end_of_candidates: true,
        username_fragment: Some("\"".repeat(MAX_UFRAG_BYTES)),
    };
    assert_eq!(
        check_candidate(&CandidateInput {
            candidate: &candidate.candidate,
            sdp_mid: candidate.sdp_mid.as_deref(),
            username_fragment: candidate.username_fragment.as_deref(),
            end_of_candidates: candidate.end_of_candidates,
        }),
        Ok(())
    );
    let mut record = start(
        call_id(),
        caller(),
        caller_dev(),
        callee(),
        offer,
        (0..10).map(|_| DeviceId::new()).collect(),
        NOW,
        &timing(),
    )
    .0;
    record.pending_ice = (0..MAX_PENDING_ICE).map(|_| candidate.clone()).collect();
    record.ice_caller = MAX_PENDING_ICE as u32;
    let json = serde_json::to_vec(&record).unwrap();
    assert!(json.len() < 100 * 1024, "{} bytes", json.len());
}

#[test]
fn records_round_trip_and_tolerate_other_versions() {
    let record = with(active(), |r| {
        r.caller_away_ms = Some(ANSWERED + 1);
        r.pending_ice = vec![cand(1)];
    });
    let json = serde_json::to_value(&record).unwrap();
    assert_eq!(json["v"], 1);
    assert_eq!(json["state"], "active");
    assert_eq!(
        serde_json::from_value::<CallRecord>(json.clone()).unwrap(),
        record
    );
    // A newer writer's extra fields are ignored.
    let mut newer = json;
    newer["participants"] = serde_json::json!(["x"]);
    assert_eq!(serde_json::from_value::<CallRecord>(newer).unwrap(), record);
    // An older writer's record without the optional fields reads with defaults.
    let older = serde_json::json!({
        "v": 1,
        "call_id": call_id(),
        "caller": caller(),
        "caller_device": caller_dev(),
        "callee": callee(),
        "state": "ringing",
        "created_ms": NOW,
        "ring_deadline_ms": DEADLINE,
        "due_ms": DEADLINE,
    });
    let read: CallRecord = serde_json::from_value(older).unwrap();
    assert_eq!(read.offer, None);
    assert!(read.ringing_devices.is_empty());
    assert!(read.pending_ice.is_empty());
    assert_eq!(read.ice_caller, 0);
}

#[test]
fn each_device_sees_its_side_of_the_call() {
    let ringing = ringing();
    let now = NOW + 5_000;
    let out = sync_view(&ringing, caller(), caller_dev(), now).unwrap();
    assert_eq!(
        out,
        CallView {
            call_id: call_id(),
            peer: callee(),
            direction: CallDirection::Out,
            status: CallState::Ringing,
            offer: None,
            expires_at_ms: Some(DEADLINE),
            expires_in_ms: Some(40_000),
            answered_ms: None,
            this_device: true,
        }
    );
    let incoming = sync_view(&ringing, callee(), d2(), now).unwrap();
    assert_eq!(incoming.direction, CallDirection::In);
    assert_eq!(incoming.peer, caller());
    assert_eq!(incoming.offer.as_deref(), Some("offer"));
    assert!(incoming.this_device);
    let other = sync_view(&ringing, callee(), d3(), now).unwrap();
    assert!(!other.this_device);
    assert_eq!(other.offer.as_deref(), Some("offer"));
    assert!(
        !sync_view(&ringing, caller(), d3(), now)
            .unwrap()
            .this_device
    );
    assert_eq!(
        sync_view(&ringing, caller(), caller_dev(), DEADLINE + 5).and_then(|v| v.expires_in_ms),
        Some(0)
    );
    assert_eq!(sync_view(&ringing, stranger(), stranger_dev(), now), None);

    let active = active();
    let answering = sync_view(&active, callee(), d1(), ANSWERED + 1).unwrap();
    assert_eq!(
        answering,
        CallView {
            call_id: call_id(),
            peer: caller(),
            direction: CallDirection::In,
            status: CallState::Active,
            offer: None,
            expires_at_ms: None,
            expires_in_ms: None,
            answered_ms: Some(ANSWERED),
            this_device: true,
        }
    );
    assert!(
        !sync_view(&active, callee(), d2(), ANSWERED + 1)
            .unwrap()
            .this_device
    );
    assert!(
        sync_view(&active, caller(), caller_dev(), ANSWERED + 1)
            .unwrap()
            .this_device
    );
}

#[test]
fn end_reasons_have_their_wire_names() {
    let names: Vec<&str> = [
        EndReason::Completed,
        EndReason::Missed,
        EndReason::Declined,
        EndReason::Cancelled,
        EndReason::Busy,
        EndReason::Failed,
        EndReason::Unreachable,
    ]
    .into_iter()
    .map(EndReason::as_str)
    .collect();
    assert_eq!(
        names,
        [
            "completed",
            "missed",
            "declined",
            "cancelled",
            "busy",
            "failed",
            "unreachable"
        ]
    );
}

#[test]
fn ringing_ends_on_no_callee_device_without_a_callee_delivery() {
    let record = with(ringing(), |r| r.ringing_devices.clear());
    let outcome = ok(transition(
        &record,
        None,
        CallEvent::Tick {
            gone: Vec::new(),
            live: Vec::new(),
        },
        DEADLINE,
        &timing(),
    ));
    assert_eq!(
        outcome.deliveries,
        vec![Delivery::Ended {
            user: caller(),
            to: vec![caller_dev()],
            reason: EndReason::Missed,
            duration_s: None,
        }]
    );
}
