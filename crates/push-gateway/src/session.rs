//! The protocol state of one WebSocket connection, without any I/O: which frames are
//! allowed when, what each one asks the gateway to do, and which entries the client has
//! already been sent.

use std::collections::HashMap;
use std::time::Duration;

use push_proto::close;
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, Deliver, Error, ErrorCode, Pong, Ready, Send};
use push_proto::{FrameError, decode_frame, id, id_bytes};
use uuid::Uuid;

/// What the gateway must do for a frame the client sent.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Send this frame to the client.
    Reply(Body),
    /// Ask message-svc whether `device` is an active device of `user`, then call
    /// [`Session::identified`] or close with [`close::INVALID_IDENTITY`].
    Identify { user: Uuid, device: Uuid },
    /// Forward a send to message-svc; answer with its `SendAck` or an error.
    Send(Send),
    /// Forward an ack to message-svc; answer only if it fails.
    Ack(Ack),
    /// Close the connection with this code and reason.
    Close(u16, &'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Connected; the first frame must be `Identify`.
    New,
    /// `Identify` is being checked.
    Identifying,
    Ready {
        device: Uuid,
    },
}

#[derive(Debug)]
pub struct Session {
    dev_auth: bool,
    state: State,
    /// The highest `seq` sent to the client per conversation, so a sync that repeats
    /// entries the client has not acked yet does not send them twice.
    sent: HashMap<Vec<u8>, i64>,
}

impl Session {
    /// A new connection. `dev_auth` is whether DEV ONLY identities are accepted.
    pub fn new(dev_auth: bool) -> Self {
        Self {
            dev_auth,
            state: State::New,
            sent: HashMap::new(),
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
                return Step::Close(close::INVALID_IDENTITY, "the first frame must be Identify");
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
                if !self.dev_auth {
                    return Step::Close(
                        close::INVALID_IDENTITY,
                        "token sign-in is not available yet",
                    );
                }
                match (id(&identify.dev_user_id), id(&identify.dev_device_id)) {
                    (Ok(user), Ok(device)) => {
                        self.state = State::Identifying;
                        Step::Identify { user, device }
                    }
                    _ => Step::Close(close::INVALID_IDENTITY, "invalid dev identity"),
                }
            }
            (_, State::New | State::Identifying) => {
                Step::Close(close::INVALID_IDENTITY, "the first frame must be Identify")
            }
            (Body::Identify(_), State::Ready { .. }) => {
                reply_error(ErrorCode::BadFrame, "already identified")
            }
            (Body::Send(send), State::Ready { .. }) => Step::Send(send),
            (Body::Ack(ack), State::Ready { .. }) => Step::Ack(ack),
            (Body::Ping(_), State::Ready { .. }) => Step::Reply(Body::Pong(Pong {})),
            (_, State::Ready { .. }) => reply_error(ErrorCode::BadFrame, "not a client frame"),
        }
    }

    /// What to do with a text message: the protocol is binary only.
    pub fn on_text(&mut self) -> Step {
        match self.state {
            State::Ready { .. } => reply_error(ErrorCode::BadFrame, "frames must be binary"),
            State::New | State::Identifying => {
                Step::Close(close::INVALID_IDENTITY, "the first frame must be Identify")
            }
        }
    }

    /// message-svc confirmed the identity: the `Ready` frame to send.
    pub fn identified(&mut self, user: Uuid, device: Uuid, heartbeat: Duration) -> Body {
        self.state = State::Ready { device };
        Body::Ready(Ready {
            user_id: id_bytes(user),
            device_id: id_bytes(device),
            heartbeat_ms: u32::try_from(heartbeat.as_millis()).unwrap_or(u32::MAX),
        })
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
}

/// An `Error` frame for the client.
pub(crate) fn error_frame(code: ErrorCode, message: &str) -> Body {
    Body::Error(Error {
        code: code.into(),
        message: message.to_owned(),
        retryable: false,
        client_msg_id: None,
    })
}

fn reply_error(code: ErrorCode, message: &str) -> Step {
    Step::Reply(error_frame(code, message))
}

#[cfg(test)]
mod tests {
    use push_proto::v1::{Identify, Ping};
    use push_proto::{MAX_FRAME_BYTES, encode_frame};

    use super::*;

    fn identify(user: Uuid, device: Uuid) -> Vec<u8> {
        encode_frame(Body::Identify(Identify {
            token: String::new(),
            dev_user_id: id_bytes(user),
            dev_device_id: id_bytes(device),
        }))
    }

    fn ready_session() -> (Session, Uuid) {
        let mut session = Session::new(true);
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(
            session.on_binary(&identify(user, device)),
            Step::Identify { user, device }
        );
        session.identified(user, device, Duration::from_secs(30));
        (session, device)
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
    fn a_dev_identity_is_checked_then_ready() {
        let mut session = Session::new(true);
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(
            session.on_binary(&identify(user, device)),
            Step::Identify { user, device }
        );
        assert_eq!(session.device(), None);
        let ready = session.identified(user, device, Duration::from_secs(30));
        assert_eq!(
            ready,
            Body::Ready(Ready {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
                heartbeat_ms: 30_000,
            })
        );
        assert_eq!(session.device(), Some(device));
    }

    #[test]
    fn without_dev_auth_identify_closes_with_4001() {
        let mut session = Session::new(false);
        assert_eq!(
            session.on_binary(&identify(Uuid::now_v7(), Uuid::now_v7())),
            Step::Close(4001, "token sign-in is not available yet")
        );
    }

    #[test]
    fn anything_but_identify_first_closes_with_4001() {
        let ping = encode_frame(Body::Ping(Ping {}));
        for bytes in [ping.as_slice(), &[0xff, 0xff], &[]] {
            let mut session = Session::new(true);
            assert_eq!(
                session.on_binary(bytes),
                Step::Close(4001, "the first frame must be Identify")
            );
        }
        assert_eq!(
            Session::new(true).on_text(),
            Step::Close(4001, "the first frame must be Identify")
        );
    }

    #[test]
    fn a_dev_identity_with_malformed_ids_closes_with_4001() {
        let mut session = Session::new(true);
        let bytes = encode_frame(Body::Identify(Identify {
            token: String::new(),
            dev_user_id: vec![1, 2, 3],
            dev_device_id: id_bytes(Uuid::now_v7()),
        }));
        assert_eq!(
            session.on_binary(&bytes),
            Step::Close(4001, "invalid dev identity")
        );
    }

    #[test]
    fn while_identifying_any_other_frame_or_a_second_identify_closes_with_4001() {
        let ping = encode_frame(Body::Ping(Ping {}));
        let second = identify(Uuid::now_v7(), Uuid::now_v7());
        for bytes in [ping.as_slice(), second.as_slice()] {
            let mut session = Session::new(true);
            let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
            assert_eq!(
                session.on_binary(&identify(user, device)),
                Step::Identify { user, device }
            );
            assert_eq!(
                session.on_binary(bytes),
                Step::Close(4001, "the first frame must be Identify")
            );
            assert_eq!(session.device(), None);
        }
    }

    #[test]
    fn an_oversized_frame_closes_before_identify_and_errors_once_ready() {
        let oversized = vec![0u8; MAX_FRAME_BYTES + 1];
        assert_eq!(
            Session::new(true).on_binary(&oversized),
            Step::Close(4001, "the first frame must be Identify")
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
        let again = identify(Uuid::now_v7(), Uuid::now_v7());
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
}
