//! The protocol state of one WebSocket connection, without any I/O: which frames are
//! allowed when, what each one asks the gateway to do, which entries the client has
//! already been sent, and when to sync again.

use std::collections::HashMap;
use std::time::Duration;

use push_proto::close;
use push_proto::v1::frame::Body;
use push_proto::v1::{
    Ack, Activity, Delete, Deliver, Edit, Error, ErrorCode, Pong, ReadUpTo, Ready, Request, Send,
    request,
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
    /// then call [`Session::identified`] or close.
    Identify(String),
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
    /// Close the connection with this code and reason.
    Close(u16, &'static str),
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
                Step::Identify(identify.token)
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
            (_, State::Ready { .. }) => reply_error(ErrorCode::BadFrame, "not a client frame"),
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
    /// names: the `Ready` frame to send. `None` if no identity is being checked.
    pub fn identified(&mut self, claims: &Claims, heartbeat: Duration) -> Option<Body> {
        if self.state != State::Identifying {
            return None;
        }
        self.state = State::Ready {
            device: claims.device,
        };
        Some(Body::Ready(Ready {
            user_id: id_bytes(claims.user),
            device_id: id_bytes(claims.device),
            heartbeat_ms: u32::try_from(heartbeat.as_millis()).unwrap_or(u32::MAX),
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
        ActivityState, AddMembers, CreateGroup, Identify, LeaveGroup, ListContacts, Ping, ReAuth,
        RemoveMember, RenameGroup, SetRole,
    };
    use push_proto::{MAX_FRAME_BYTES, encode_frame};

    use super::*;

    fn identify(token: &str) -> Vec<u8> {
        encode_frame(Body::Identify(Identify {
            token: token.to_owned(),
        }))
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
            Step::Identify("token".to_owned())
        );
        let claims = claims();
        session
            .identified(&claims, Duration::from_secs(30))
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
            Step::Identify("a.b.c".to_owned())
        );
        assert_eq!(session.device(), None);
        let claims = claims();
        let ready = session.identified(&claims, Duration::from_secs(30));
        assert_eq!(
            ready,
            Some(Body::Ready(Ready {
                user_id: id_bytes(claims.user),
                device_id: id_bytes(claims.device),
                heartbeat_ms: 30_000,
            }))
        );
        assert_eq!(session.device(), Some(claims.device));
        assert_eq!(session.identified(&claims, Duration::from_secs(30)), None);
    }

    #[test]
    fn identified_without_a_pending_identity_does_nothing() {
        let mut session = Session::new(false);
        assert_eq!(session.identified(&claims(), Duration::from_secs(30)), None);
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
                Step::Identify("token".to_owned())
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
        session.identified(&claims(), Duration::from_secs(30));
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
