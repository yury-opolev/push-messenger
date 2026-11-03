//! The protocol state of one WebSocket connection, without any I/O: which frames are
//! allowed when, what each one asks the gateway to do, which entries the client has
//! already been sent, whether calls are active, and when to sync again.

use std::collections::HashMap;
use std::time::Duration;

use push_proto::close;
use push_proto::internal::v1::call_request;
use push_proto::v1::frame::Body;
use push_proto::v1::{
    Ack, Activity, CallAccept, CallCancel, CallDecline, CallError, CallHangup, CallIce, CallInvite,
    CallRenegotiate, Capability, Delete, Deliver, Edit, Error, ErrorCode, Pong, ReadUpTo, Ready,
    Request, Send, request,
};
use push_proto::{FrameError, decode_frame, id_bytes};
use uuid::Uuid;

use crate::tokens::Claims;

/// What the gateway must do for a frame the client sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Send this frame to the client.
    Reply(Body),
    /// Check this access token, ask message-svc whether the device it names may connect,
    /// then call [`Session::identified`] or close. `capabilities`: what the client
    /// advertised (`Capability` values, unknown ones included).
    Identify {
        token: String,
        capabilities: Vec<i32>,
    },
    /// Check this access token, which must name the connection's user and device, and
    /// ask message-svc again: the connection continues on the new token.
    ReAuth(String),
    /// Forward a send to message-svc; answer with its `SendAck` or an error.
    Send(Send),
    /// Forward an ack to message-svc; answer only if it fails. If it moved the position,
    /// ask [`Session::sync_after_ack`] whether to sync again.
    Ack(Ack),
    /// Forward an edit to message-svc; answer with its `SendAck` or an error.
    Edit(Edit),
    /// Forward a delete to message-svc; answer with its `SendAck` or an error.
    Delete(Delete),
    /// Forward a read position to message-svc; answer only if it fails (the user's
    /// devices hear the new position as `ReadPosition`).
    ReadUpTo(ReadUpTo),
    /// Forward a request to message-svc; answer with its `Response` or an error, both
    /// carrying the request's `request_id`.
    Request(Request),
    /// Pace an activity through the connection's [`crate::ActivityLimiter`], then forward
    /// it to message-svc; answer only if it fails, with an error that is never retryable.
    Activity(Activity),
    /// Forward a call frame of a calls-active connection to message-svc (`Messaging.Call`);
    /// answer a refusal with `CallError`. Its `Debug` never shows the frame.
    Call(CallFrame),
    /// Close the connection with this code and reason.
    Close(u16, &'static str),
}

/// A call frame a client sent (docs/calls.md). SDP and candidates are never logged, so
/// `Debug` shows only which frame it is.
#[derive(Clone, PartialEq)]
pub enum CallFrame {
    Invite(CallInvite),
    Accept(CallAccept),
    Decline(CallDecline),
    Cancel(CallCancel),
    Ice(CallIce),
    Hangup(CallHangup),
    Renegotiate(CallRenegotiate),
}

impl CallFrame {
    /// The call the frame is about.
    pub fn call_id(&self) -> &[u8] {
        match self {
            Self::Invite(frame) => &frame.call_id,
            Self::Accept(frame) => &frame.call_id,
            Self::Decline(frame) => &frame.call_id,
            Self::Cancel(frame) => &frame.call_id,
            Self::Ice(frame) => &frame.call_id,
            Self::Hangup(frame) => &frame.call_id,
            Self::Renegotiate(frame) => &frame.call_id,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Invite(_) => "Invite",
            Self::Accept(_) => "Accept",
            Self::Decline(_) => "Decline",
            Self::Cancel(_) => "Cancel",
            Self::Ice(_) => "Ice",
            Self::Hangup(_) => "Hangup",
            Self::Renegotiate(_) => "Renegotiate",
        }
    }
}

impl std::fmt::Debug for CallFrame {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "CallFrame::{}(..)", self.name())
    }
}

impl From<CallFrame> for call_request::Frame {
    fn from(frame: CallFrame) -> Self {
        match frame {
            CallFrame::Invite(frame) => Self::Invite(frame),
            CallFrame::Accept(frame) => Self::Accept(frame),
            CallFrame::Decline(frame) => Self::Decline(frame),
            CallFrame::Cancel(frame) => Self::Cancel(frame),
            CallFrame::Ice(frame) => Self::Ice(frame),
            CallFrame::Hangup(frame) => Self::Hangup(frame),
            CallFrame::Renegotiate(frame) => Self::Renegotiate(frame),
        }
    }
}

/// The client frame as a call frame, if it is one.
fn call_frame(body: Body) -> Option<CallFrame> {
    match body {
        Body::CallInvite(frame) => Some(CallFrame::Invite(frame)),
        Body::CallAccept(frame) => Some(CallFrame::Accept(frame)),
        Body::CallDecline(frame) => Some(CallFrame::Decline(frame)),
        Body::CallCancel(frame) => Some(CallFrame::Cancel(frame)),
        Body::CallIce(frame) => Some(CallFrame::Ice(frame)),
        Body::CallHangup(frame) => Some(CallFrame::Hangup(frame)),
        Body::CallRenegotiate(frame) => Some(CallFrame::Renegotiate(frame)),
        _ => None,
    }
}

/// Whether `body` is a server call frame.
fn is_call_body(body: &Body) -> bool {
    matches!(
        body,
        Body::CallIncoming(_)
            | Body::CallAccepted(_)
            | Body::CallTaken(_)
            | Body::CallEnded(_)
            | Body::CallSync(_)
            | Body::PeerCallIce(_)
            | Body::PeerCallRenegotiate(_)
            | Body::CallError(_)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Connected; the first frame must be `Identify`.
    New,
    /// The token of `Identify` is being checked.
    Identifying,
    Ready {
        device: Uuid,
    },
}

#[derive(Debug)]
pub struct Session {
    /// Whether group requests are served (`PUSH_GROUPS`); refused with `NOT_AVAILABLE`
    /// otherwise.
    groups: bool,
    /// Whether the connection is calls-active: the server offered calls in `Ready` and
    /// the client advertised them in `Identify`. Call frames are refused otherwise.
    calls_active: bool,
    state: State,
    /// The highest `seq` sent to the client per conversation, so a sync that repeats
    /// entries the client has not acked yet does not send them twice.
    sent: HashMap<Vec<u8>, i64>,
    /// The last sync left entries behind (`more`). They come only after an ack, which
    /// rings nobody, so the session must sync again itself.
    more_waiting: bool,
}

impl Session {
    /// A new connection. `groups`: whether group requests are served.
    pub fn new(groups: bool) -> Self {
        Self {
            groups,
            calls_active: false,
            state: State::New,
            sent: HashMap::new(),
            more_waiting: false,
        }
    }

    /// The connected device, once identified.
    pub fn device(&self) -> Option<Uuid> {
        match self.state {
            State::Ready { device } => Some(device),
            State::New | State::Identifying => None,
        }
    }

    /// Whether the connection is calls-active.
    pub fn calls_active(&self) -> bool {
        self.calls_active
    }

    /// Whether a frame from the hub goes out on this connection: call frames only reach
    /// calls-active connections.
    pub fn forwards(&self, body: &Body) -> bool {
        self.calls_active || !is_call_body(body)
    }

    /// What to do with a binary message from the client.
    pub fn on_binary(&mut self, bytes: &[u8]) -> Step {
        let body = match (decode_frame(bytes), self.state) {
            (Ok(body), _) => body,
            (Err(_), State::New | State::Identifying) => {
                return Step::Close(close::PROTOCOL_ERROR, "the first frame must be Identify");
            }
            (Err(FrameError::TooLarge { .. }), State::Ready { .. }) => {
                return reply_error(ErrorCode::BadFrame, "the frame is too large");
            }
            (Err(_), State::Ready { .. }) => {
                return reply_error(ErrorCode::BadFrame, "the frame could not be decoded");
            }
        };
        match (body, self.state) {
            (Body::Identify(identify), State::New) => {
                self.state = State::Identifying;
                Step::Identify {
                    token: identify.token,
                    capabilities: identify.capabilities,
                }
            }
            (_, State::New | State::Identifying) => {
                Step::Close(close::PROTOCOL_ERROR, "the first frame must be Identify")
            }
            (Body::Identify(_), State::Ready { .. }) => {
                reply_error(ErrorCode::BadFrame, "already identified")
            }
            (Body::ReAuth(reauth), State::Ready { .. }) => Step::ReAuth(reauth.token),
            (Body::Send(send), State::Ready { .. }) => Step::Send(send),
            (Body::Ack(ack), State::Ready { .. }) => Step::Ack(ack),
            (Body::Edit(edit), State::Ready { .. }) => Step::Edit(edit),
            (Body::Delete(delete), State::Ready { .. }) => Step::Delete(delete),
            (Body::ReadUpTo(read), State::Ready { .. }) => Step::ReadUpTo(read),
            (Body::Request(request), State::Ready { .. }) => {
                if !self.groups && request.body.as_ref().is_some_and(is_group_request) {
                    return Step::Reply(Body::Error(Error {
                        code: ErrorCode::NotAvailable.into(),
                        message: "groups are not available".to_owned(),
                        retryable: false,
                        client_msg_id: None,
                        request_id: Some(request.request_id),
                        retry_after_s: None,
                    }));
                }
                Step::Request(request)
            }
            (Body::Activity(activity), State::Ready { .. }) => Step::Activity(activity),
            (Body::Ping(_), State::Ready { .. }) => Step::Reply(Body::Pong(Pong {})),
            (body, State::Ready { .. }) => match call_frame(body) {
                Some(frame) if self.calls_active => Step::Call(frame),
                Some(frame) => Step::Reply(Body::CallError(CallError {
                    call_id: frame.call_id().to_vec(),
                    code: ErrorCode::NotAvailable.into(),
                    message: "calls are not available on this connection".to_owned(),
                    retryable: false,
                    retry_after_s: None,
                })),
                None => reply_error(ErrorCode::BadFrame, "not a client frame"),
            },
        }
    }

    /// What to do with a text message: the protocol is binary only.
    pub fn on_text(&mut self) -> Step {
        match self.state {
            State::Ready { .. } => reply_error(ErrorCode::BadFrame, "frames must be binary"),
            State::New | State::Identifying => {
                Step::Close(close::PROTOCOL_ERROR, "the first frame must be Identify")
            }
        }
    }

    /// The token of [`Step::Identify`] passed and message-svc accepted the device it
    /// names: the `Ready` frame to send, listing what the server `offered` on this
    /// connection. `calls_active`: whether call frames are served. `None` if no identity
    /// is being checked.
    pub fn identified(
        &mut self,
        claims: &Claims,
        heartbeat: Duration,
        offered: &[Capability],
        calls_active: bool,
    ) -> Option<Body> {
        if self.state != State::Identifying {
            return None;
        }
        self.state = State::Ready {
            device: claims.device,
        };
        self.calls_active = calls_active;
        Some(Body::Ready(Ready {
            user_id: id_bytes(claims.user),
            device_id: id_bytes(claims.device),
            heartbeat_ms: u32::try_from(heartbeat.as_millis()).unwrap_or(u32::MAX),
            capabilities: offered
                .iter()
                .map(|&capability| capability.into())
                .collect(),
        }))
    }

    /// The entries of a sync the client has not been sent on this connection yet,
    /// remembering them as sent.
    pub fn fresh(&mut self, entries: Vec<Deliver>) -> Vec<Deliver> {
        entries
            .into_iter()
            .filter(|entry| {
                let sent = self.sent.entry(entry.conversation_id.clone()).or_insert(0);
                if entry.seq > *sent {
                    *sent = entry.seq;
                    true
                } else {
                    false
                }
            })
            .collect()
    }

    /// Records whether the last sync left entries behind.
    pub fn synced(&mut self, more: bool) {
        self.more_waiting = more;
    }

    /// Whether to sync again after an ack: only if the ack `moved` the device's position
    /// and the last sync left entries behind. Then the next sync returns them.
    pub fn sync_after_ack(&self, moved: bool) -> bool {
        moved && self.more_waiting
    }
}

/// The requests about groups, which apps cannot use in the first release (owner
/// decision): message-svc keeps serving them, the gateway refuses these six
/// group-management requests unless `PUSH_GROUPS=1`. It refuses nothing else: groups that
/// already exist (made while the switch was on, or directly through message-svc) still
/// reach clients through `Deliver` and `Sync`, listings, history, group receipts, and
/// `Send`, `Edit`, `Delete` and `ReadUpTo`.
fn is_group_request(body: &request::Body) -> bool {
    matches!(
        body,
        request::Body::CreateGroup(_)
            | request::Body::AddMembers(_)
            | request::Body::RemoveMember(_)
            | request::Body::LeaveGroup(_)
            | request::Body::RenameGroup(_)
            | request::Body::SetRole(_)
    )
}

/// When to retry a failed sync: after 1 s, doubling up to 30 s, and from 1 s again once a
/// sync succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryDelay {
    next: Duration,
}

impl RetryDelay {
    pub const FIRST: Duration = Duration::from_secs(1);
    pub const MAX: Duration = Duration::from_secs(30);

    /// A sync failed: how long to wait before the next attempt.
    pub fn failed(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(Self::MAX);
        delay
    }

    /// A sync succeeded.
    pub fn succeeded(&mut self) {
        self.next = Self::FIRST;
    }
}

impl Default for RetryDelay {
    fn default() -> Self {
        Self { next: Self::FIRST }
    }
}

/// An `Error` frame for the client.
pub(crate) fn error_frame(code: ErrorCode, message: &str) -> Body {
    Body::Error(Error {
        code: code.into(),
        message: message.to_owned(),
        retryable: false,
        client_msg_id: None,
        request_id: None,
        retry_after_s: None,
    })
}

fn reply_error(code: ErrorCode, message: &str) -> Step {
    Step::Reply(error_frame(code, message))
}

#[cfg(test)]
mod tests {
    use push_proto::v1::{
        ActivityState, AddMembers, CallAccepted, CallEnded, CallIncoming, CallSync, CallTaken,
        CreateGroup, Identify, LeaveGroup, ListContacts, Ping, ReAuth, RemoveMember, RenameGroup,
        SetRole,
    };
    use push_proto::{MAX_FRAME_BYTES, encode_frame};

    use super::*;

    fn identify(token: &str) -> Vec<u8> {
        encode_frame(Body::Identify(Identify {
            token: token.to_owned(),
            capabilities: Vec::new(),
        }))
    }

    fn identify_step(token: &str) -> Step {
        Step::Identify {
            token: token.to_owned(),
            capabilities: Vec::new(),
        }
    }

    fn claims() -> Claims {
        Claims {
            user: Uuid::now_v7(),
            device: Uuid::now_v7(),
            issued_at: 1_700_000_000,
            expires_at: 1_700_000_900,
        }
    }

    fn ready_session() -> (Session, Uuid) {
        let mut session = Session::new(false);
        assert_eq!(
            session.on_binary(&identify("token")),
            identify_step("token")
        );
        let claims = claims();
        session
            .identified(&claims, Duration::from_secs(30), &[], false)
            .unwrap();
        (session, claims.device)
    }

    fn error_code(step: Step) -> ErrorCode {
        match step {
            Step::Reply(Body::Error(error)) => error.code(),
            other => panic!("expected an error frame, got {other:?}"),
        }
    }

    fn entry(conversation: &[u8], seq: i64) -> Deliver {
        Deliver {
            conversation_id: conversation.to_vec(),
            seq,
            ..Deliver::default()
        }
    }

    #[test]
    fn identify_hands_over_its_token_and_ready_names_the_tokens_device() {
        let mut session = Session::new(false);
        assert_eq!(
            session.on_binary(&identify("a.b.c")),
            identify_step("a.b.c")
        );
        assert_eq!(session.device(), None);
        let claims = claims();
        let ready = session.identified(&claims, Duration::from_secs(30), &[], false);
        assert_eq!(
            ready,
            Some(Body::Ready(Ready {
                user_id: id_bytes(claims.user),
                device_id: id_bytes(claims.device),
                heartbeat_ms: 30_000,
                capabilities: Vec::new(),
            }))
        );
        assert_eq!(session.device(), Some(claims.device));
        assert_eq!(
            session.identified(&claims, Duration::from_secs(30), &[], false),
            None
        );
    }

    #[test]
    fn identified_without_a_pending_identity_does_nothing() {
        let mut session = Session::new(false);
        assert_eq!(
            session.identified(&claims(), Duration::from_secs(30), &[], false),
            None
        );
        assert_eq!(session.device(), None);
    }

    /// A protocol mistake before Ready is no reason to sign out: 1002, not 4001.
    #[test]
    fn anything_but_identify_first_is_a_protocol_error() {
        let ping = encode_frame(Body::Ping(Ping {}));
        for bytes in [ping.as_slice(), &[0xff, 0xff], &[]] {
            let mut session = Session::new(false);
            assert_eq!(
                session.on_binary(bytes),
                Step::Close(1002, "the first frame must be Identify")
            );
        }
        assert_eq!(
            Session::new(false).on_text(),
            Step::Close(1002, "the first frame must be Identify")
        );
    }

    #[test]
    fn while_identifying_any_other_frame_or_a_second_identify_is_a_protocol_error() {
        let ping = encode_frame(Body::Ping(Ping {}));
        let second = identify("another");
        for bytes in [ping.as_slice(), second.as_slice()] {
            let mut session = Session::new(false);
            assert_eq!(
                session.on_binary(&identify("token")),
                identify_step("token")
            );
            assert_eq!(
                session.on_binary(bytes),
                Step::Close(1002, "the first frame must be Identify")
            );
            assert_eq!(session.device(), None);
        }
    }

    #[test]
    fn reauth_hands_over_its_token_once_ready_only() {
        let reauth = encode_frame(Body::ReAuth(ReAuth {
            token: "fresh".to_owned(),
        }));
        assert_eq!(
            Session::new(false).on_binary(&reauth),
            Step::Close(1002, "the first frame must be Identify")
        );
        let (mut session, _) = ready_session();
        assert_eq!(session.on_binary(&reauth), Step::ReAuth("fresh".to_owned()));
    }

    #[test]
    fn an_oversized_frame_closes_before_identify_and_errors_once_ready() {
        let oversized = vec![0u8; MAX_FRAME_BYTES + 1];
        assert_eq!(
            Session::new(false).on_binary(&oversized),
            Step::Close(1002, "the first frame must be Identify")
        );
        let (mut session, _) = ready_session();
        assert_eq!(
            session.on_binary(&oversized),
            Step::Reply(error_frame(ErrorCode::BadFrame, "the frame is too large"))
        );
        assert!(session.device().is_some());
    }
    #[test]
    fn once_ready_frames_become_steps() {
        let (mut session, _) = ready_session();
        assert_eq!(
            session.on_binary(&encode_frame(Body::Ping(Ping {}))),
            Step::Reply(Body::Pong(Pong {}))
        );
        let send = Send {
            client_msg_id: id_bytes(Uuid::now_v7()),
            ..Send::default()
        };
        assert_eq!(
            session.on_binary(&encode_frame(Body::Send(send.clone()))),
            Step::Send(send)
        );
        let ack = Ack {
            conversation_id: id_bytes(Uuid::now_v7()),
            seq: 3,
        };
        assert_eq!(
            session.on_binary(&encode_frame(Body::Ack(ack.clone()))),
            Step::Ack(ack)
        );
    }

    #[test]
    fn once_ready_bad_frames_get_errors_but_keep_the_connection() {
        let (mut session, _) = ready_session();
        let again = identify("again");
        assert_eq!(error_code(session.on_binary(&again)), ErrorCode::BadFrame);
        assert_eq!(
            error_code(session.on_binary(&[0xff, 0xff])),
            ErrorCode::BadFrame
        );
        let pong = encode_frame(Body::Pong(Pong {}));
        assert_eq!(error_code(session.on_binary(&pong)), ErrorCode::BadFrame);
        assert_eq!(error_code(session.on_text()), ErrorCode::BadFrame);
        assert!(session.device().is_some());
    }

    #[test]
    fn edits_deletes_reads_and_requests_become_steps() {
        let (mut session, _) = ready_session();
        let edit = Edit {
            client_msg_id: id_bytes(Uuid::now_v7()),
            ..Edit::default()
        };
        assert_eq!(
            session.on_binary(&encode_frame(Body::Edit(edit.clone()))),
            Step::Edit(edit)
        );
        let delete = Delete {
            client_msg_id: id_bytes(Uuid::now_v7()),
            ..Delete::default()
        };
        assert_eq!(
            session.on_binary(&encode_frame(Body::Delete(delete.clone()))),
            Step::Delete(delete)
        );
        let read = ReadUpTo {
            conversation_id: id_bytes(Uuid::now_v7()),
            seq: 4,
        };
        assert_eq!(
            session.on_binary(&encode_frame(Body::ReadUpTo(read.clone()))),
            Step::ReadUpTo(read)
        );
        let list = Request {
            request_id: 7,
            body: Some(request::Body::ListContacts(ListContacts {})),
        };
        assert_eq!(
            session.on_binary(&encode_frame(Body::Request(list.clone()))),
            Step::Request(list)
        );
    }

    #[test]
    fn activity_becomes_a_step_once_ready() {
        let activity = Activity {
            conversation_id: id_bytes(Uuid::now_v7()),
            state: ActivityState::Working.into(),
            label: Some("running tests".to_owned()),
        };
        let frame = encode_frame(Body::Activity(activity.clone()));
        assert_eq!(
            Session::new(false).on_binary(&frame),
            Step::Close(1002, "the first frame must be Identify")
        );
        let (mut session, _) = ready_session();
        assert_eq!(session.on_binary(&frame), Step::Activity(activity));
    }

    fn group_requests() -> Vec<request::Body> {
        let conversation_id = id_bytes(Uuid::now_v7());
        vec![
            request::Body::CreateGroup(CreateGroup::default()),
            request::Body::AddMembers(AddMembers::default()),
            request::Body::RemoveMember(RemoveMember::default()),
            request::Body::LeaveGroup(LeaveGroup { conversation_id }),
            request::Body::RenameGroup(RenameGroup::default()),
            request::Body::SetRole(SetRole::default()),
        ]
    }

    #[test]
    fn group_requests_are_refused_unless_groups_are_switched_on() {
        let (mut session, _) = ready_session();
        for (number, body) in (1..).zip(group_requests()) {
            let request = Request {
                request_id: number,
                body: Some(body),
            };
            let Step::Reply(Body::Error(error)) =
                session.on_binary(&encode_frame(Body::Request(request)))
            else {
                panic!("a group request was not refused");
            };
            assert_eq!(error.code(), ErrorCode::NotAvailable);
            assert_eq!(error.request_id, Some(number));
            assert!(!error.retryable);
        }

        let mut session = Session::new(true);
        session.on_binary(&identify("token"));
        session.identified(&claims(), Duration::from_secs(30), &[], false);
        for body in group_requests() {
            let request = Request {
                request_id: 1,
                body: Some(body),
            };
            assert_eq!(
                session.on_binary(&encode_frame(Body::Request(request.clone()))),
                Step::Request(request)
            );
        }
    }

    fn call_frames() -> Vec<Body> {
        let call_id = id_bytes(Uuid::now_v7());
        vec![
            Body::CallInvite(CallInvite {
                call_id: call_id.clone(),
                ..CallInvite::default()
            }),
            Body::CallAccept(CallAccept {
                call_id: call_id.clone(),
                ..CallAccept::default()
            }),
            Body::CallDecline(CallDecline {
                call_id: call_id.clone(),
                reason: None,
            }),
            Body::CallCancel(CallCancel {
                call_id: call_id.clone(),
            }),
            Body::CallIce(CallIce {
                call_id: call_id.clone(),
                ..CallIce::default()
            }),
            Body::CallHangup(CallHangup {
                call_id: call_id.clone(),
                reason: None,
            }),
            Body::CallRenegotiate(CallRenegotiate {
                call_id,
                ..CallRenegotiate::default()
            }),
        ]
    }

    fn calls_session(offered: &[Capability], calls_active: bool) -> (Session, Option<Body>) {
        let mut session = Session::new(false);
        let hello = encode_frame(Body::Identify(Identify {
            token: "token".to_owned(),
            capabilities: vec![Capability::Calls.into()],
        }));
        assert_eq!(
            session.on_binary(&hello),
            Step::Identify {
                token: "token".to_owned(),
                capabilities: vec![Capability::Calls.into()],
            }
        );
        let ready = session.identified(&claims(), Duration::from_secs(30), offered, calls_active);
        (session, ready)
    }

    #[test]
    fn call_frames_from_the_hub_only_reach_calls_active_connections() {
        let call_bodies = [
            Body::CallIncoming(CallIncoming::default()),
            Body::CallAccepted(CallAccepted::default()),
            Body::CallTaken(CallTaken::default()),
            Body::CallEnded(CallEnded::default()),
            Body::CallError(CallError::default()),
            Body::CallSync(CallSync::default()),
            Body::PeerCallIce(CallIce::default()),
            Body::PeerCallRenegotiate(CallRenegotiate::default()),
        ];
        let other = Body::Ready(Default::default());
        let (active, _) = calls_session(&[Capability::Calls], true);
        let (inactive, _) = calls_session(&[], false);
        for body in &call_bodies {
            assert!(active.forwards(body), "{body:?}");
            assert!(!inactive.forwards(body), "{body:?}");
        }
        assert!(active.forwards(&other));
        assert!(inactive.forwards(&other));
    }

    #[test]
    fn ready_lists_what_the_server_offered() {
        let (session, ready) = calls_session(&[Capability::Calls], true);
        let Some(Body::Ready(ready)) = ready else {
            panic!("no Ready");
        };
        assert_eq!(ready.capabilities, vec![i32::from(Capability::Calls)]);
        assert!(session.calls_active());
        let (session, ready) = calls_session(&[], false);
        let Some(Body::Ready(ready)) = ready else {
            panic!("no Ready");
        };
        assert!(ready.capabilities.is_empty());
        assert!(!session.calls_active());
    }

    #[test]
    fn call_frames_become_steps_on_a_calls_active_connection() {
        let (mut session, _) = calls_session(&[Capability::Calls], true);
        for body in call_frames() {
            let expected = call_frame(body.clone()).unwrap();
            let step = session.on_binary(&encode_frame(body));
            assert_eq!(step, Step::Call(expected.clone()));
            let Step::Call(frame) = step else {
                unreachable!();
            };
            assert_eq!(frame.call_id(), expected.call_id());
            assert!(!format!("{frame:?}").contains("call_id"));
        }
    }

    #[test]
    fn call_frames_are_not_available_on_other_connections() {
        // Offered but not advertised by the client, or not offered at all.
        for offered in [&[Capability::Calls][..], &[]] {
            let (mut session, _) = calls_session(offered, false);
            for body in call_frames() {
                let call_id = call_frame(body.clone()).unwrap().call_id().to_vec();
                assert_eq!(
                    session.on_binary(&encode_frame(body)),
                    Step::Reply(Body::CallError(CallError {
                        call_id,
                        code: ErrorCode::NotAvailable.into(),
                        message: "calls are not available on this connection".to_owned(),
                        retryable: false,
                        retry_after_s: None,
                    }))
                );
            }
        }
    }

    #[test]
    fn server_call_frames_from_a_client_are_bad_frames() {
        let (mut session, _) = calls_session(&[Capability::Calls], true);
        for body in [
            Body::CallIncoming(CallIncoming::default()),
            Body::CallAccepted(CallAccepted::default()),
            Body::CallTaken(CallTaken::default()),
            Body::CallEnded(CallEnded::default()),
            Body::CallError(CallError::default()),
            Body::CallSync(CallSync::default()),
            Body::PeerCallIce(CallIce::default()),
            Body::PeerCallRenegotiate(CallRenegotiate::default()),
        ] {
            assert_eq!(
                error_code(session.on_binary(&encode_frame(body))),
                ErrorCode::BadFrame
            );
        }
    }

    #[test]
    fn a_call_frame_becomes_the_internal_frame() {
        let invite = CallInvite {
            call_id: id_bytes(Uuid::now_v7()),
            ..CallInvite::default()
        };
        assert_eq!(
            call_request::Frame::from(CallFrame::Invite(invite.clone())),
            call_request::Frame::Invite(invite)
        );
        let hangup = CallHangup {
            call_id: id_bytes(Uuid::now_v7()),
            reason: None,
        };
        assert_eq!(
            call_request::Frame::from(CallFrame::Hangup(hangup.clone())),
            call_request::Frame::Hangup(hangup)
        );
    }

    #[test]
    fn entries_already_sent_are_not_sent_again() {
        let (mut session, _) = ready_session();
        let (a, b) = (id_bytes(Uuid::now_v7()), id_bytes(Uuid::now_v7()));
        assert_eq!(
            session.fresh(vec![entry(&a, 1), entry(&a, 2), entry(&b, 1)]),
            vec![entry(&a, 1), entry(&a, 2), entry(&b, 1)]
        );
        assert_eq!(
            session.fresh(vec![entry(&a, 1), entry(&a, 2), entry(&a, 3), entry(&b, 1)]),
            vec![entry(&a, 3)]
        );
    }

    #[test]
    fn an_ack_syncs_again_only_if_it_moved_and_entries_were_left() {
        let (mut session, _) = ready_session();
        assert!(!session.sync_after_ack(true), "nothing was left behind");
        session.synced(true);
        assert!(session.sync_after_ack(true));
        assert!(!session.sync_after_ack(false), "the position did not move");
        session.synced(false);
        assert!(!session.sync_after_ack(true));
    }

    #[test]
    fn failed_syncs_back_off_from_1_s_to_30_s_and_reset_on_success() {
        let mut delay = RetryDelay::default();
        let delays: Vec<u64> = (0..7).map(|_| delay.failed().as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
        delay.succeeded();
        assert_eq!(delay.failed(), Duration::from_secs(1));
    }
}
