//! Encoding frames, identifiers and errors.

use prost::Message;
use uuid::Uuid;

use crate::v1::{Error, ErrorCode, Frame, frame::Body};

/// Largest WebSocket message a gateway accepts: a text body of the maximum size
/// (64 KiB, `push_domain::MAX_TEXT_BYTES`) with room for the other fields.
pub const MAX_FRAME_BYTES: usize = 128 * 1024;

/// Why a WebSocket message is not a usable frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("a frame is at most {limit} bytes, not {size}")]
    TooLarge { size: usize, limit: usize },
    #[error("not a protobuf frame: {0}")]
    Malformed(String),
    #[error("the frame has no body")]
    Empty,
}

/// An identifier field that is not a 16-byte UUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("an id must be 16 bytes, not {0}")]
pub struct InvalidId(pub usize);

/// One frame, ready to send as a WebSocket binary message.
pub fn encode_frame(body: Body) -> Vec<u8> {
    Frame { body: Some(body) }.encode_to_vec()
}

/// The body of a received WebSocket binary message.
pub fn decode_frame(bytes: &[u8]) -> Result<Body, FrameError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            size: bytes.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    let frame = Frame::decode(bytes).map_err(|error| FrameError::Malformed(error.to_string()))?;
    frame.body.ok_or(FrameError::Empty)
}

/// Reads an identifier field.
pub fn id(bytes: &[u8]) -> Result<Uuid, InvalidId> {
    Uuid::from_slice(bytes).map_err(|_| InvalidId(bytes.len()))
}

/// Writes an identifier field.
pub fn id_bytes(id: Uuid) -> Vec<u8> {
    id.as_bytes().to_vec()
}

/// The Redis channel message-svc publishes a user's
/// [`Notification`](crate::internal::v1::Notification)s on, and the gateways holding the
/// user's devices subscribe to: `notify:{user_id}`.
pub fn notify_channel(user: Uuid) -> String {
    format!("notify:{user}")
}

/// The Redis channel message-svc publishes a device's
/// [`Kick`](crate::internal::v1::Kick) on, and the gateway holding the device subscribes
/// to: `kick:{device_id}`.
pub fn kick_channel(device: Uuid) -> String {
    format!("kick:{device}")
}

/// A gRPC status carrying `error` for the client, as message-svc answers.
///
/// The details (`grpc-status-details-bin`) follow gRPC's rich error model: a
/// `google.rpc.Status` whose `details` hold the `push.v1.Error` as a `google.protobuf.Any`
/// (type URL [`ERROR_TYPE_URL`]), so any gRPC tool can read them.
pub fn error_status(code: tonic::Code, error: Error) -> tonic::Status {
    let message = error.message.clone();
    let details = RpcStatus {
        code: code as i32,
        message: message.clone(),
        details: vec![Any {
            type_url: ERROR_TYPE_URL.to_owned(),
            value: error.encode_to_vec(),
        }],
    };
    tonic::Status::with_details(code, message, details.encode_to_vec().into())
}

/// The client error a gRPC status carries. A status without one — message-svc is
/// unreachable, or failed before answering — becomes a retryable `UNAVAILABLE`.
pub fn error_from_status(status: &tonic::Status) -> Error {
    let carried = RpcStatus::decode(status.details())
        .ok()
        .and_then(|rich| {
            rich.details
                .into_iter()
                .find(|detail| detail.type_url == ERROR_TYPE_URL)
        })
        .and_then(|detail| Error::decode(detail.value.as_slice()).ok());
    match carried {
        Some(error) if error.code() != ErrorCode::Unspecified => error,
        _ => Error {
            code: ErrorCode::Unavailable.into(),
            message: "the message service is unavailable".to_owned(),
            retryable: true,
            client_msg_id: None,
            request_id: None,
            retry_after_s: None,
        },
    }
}

/// The type URL of a `push.v1.Error` packed in a `google.protobuf.Any`.
pub const ERROR_TYPE_URL: &str = "type.googleapis.com/push.v1.Error";

/// `google.rpc.Status`: what gRPC's rich error model puts in `grpc-status-details-bin`.
#[derive(Clone, PartialEq, prost::Message)]
struct RpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<Any>,
}

/// `google.protobuf.Any`.
#[derive(Clone, PartialEq, prost::Message)]
struct Any {
    #[prost(string, tag = "1")]
    type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    value: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1::{ContentType, Ping, Send};

    #[test]
    fn a_frame_survives_encoding() {
        let send = Body::Send(Send {
            client_msg_id: id_bytes(Uuid::now_v7()),
            conversation_id: id_bytes(Uuid::now_v7()),
            content_type: ContentType::Text.into(),
            body: b"hello".to_vec(),
            client_ts: Some(1_700_000_000_000_000),
        });
        assert_eq!(decode_frame(&encode_frame(send.clone())), Ok(send));
        assert_eq!(
            decode_frame(&encode_frame(Body::Ping(Ping {}))),
            Ok(Body::Ping(Ping {}))
        );
    }

    #[test]
    fn the_request_and_server_frames_survive_encoding() {
        use crate::v1::{
            ContactEvent, ContactState, DirectReceipt, ListContacts, ReAuth, ReadPosition, Receipt,
            Request, Response, receipt, request, response,
        };
        let conversation = id_bytes(Uuid::now_v7());
        let frames = [
            Body::ReAuth(ReAuth {
                token: "a.b.c".to_owned(),
            }),
            Body::Request(Request {
                request_id: 7,
                body: Some(request::Body::ListContacts(ListContacts {})),
            }),
            Body::Response(Response {
                request_id: 7,
                body: Some(response::Body::Done(crate::v1::Done {})),
            }),
            Body::Receipt(Receipt {
                conversation_id: conversation.clone(),
                positions: Some(receipt::Positions::Direct(DirectReceipt {
                    user_id: id_bytes(Uuid::now_v7()),
                    delivered_seq: 4,
                    read_seq: 3,
                })),
            }),
            Body::ReadPosition(ReadPosition {
                conversation_id: conversation,
                read_seq: 3,
            }),
            Body::ContactEvent(ContactEvent {
                user_id: id_bytes(Uuid::now_v7()),
                state: ContactState::Accepted.into(),
            }),
        ];
        for frame in frames {
            assert_eq!(decode_frame(&encode_frame(frame.clone())), Ok(frame));
        }
    }

    #[test]
    fn a_frame_without_a_body_is_empty() {
        assert_eq!(decode_frame(&[]), Err(FrameError::Empty));
    }

    #[test]
    fn bytes_that_are_not_protobuf_are_malformed() {
        assert!(matches!(
            decode_frame(&[0xff, 0xff, 0xff]),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn a_frame_over_the_limit_is_refused_before_decoding() {
        let bytes = vec![0; MAX_FRAME_BYTES + 1];
        assert_eq!(
            decode_frame(&bytes),
            Err(FrameError::TooLarge {
                size: MAX_FRAME_BYTES + 1,
                limit: MAX_FRAME_BYTES,
            })
        );
    }

    #[test]
    fn an_id_is_exactly_16_bytes() {
        let uuid = Uuid::now_v7();
        assert_eq!(id(&id_bytes(uuid)), Ok(uuid));
        assert_eq!(id(&[1; 15]), Err(InvalidId(15)));
        assert_eq!(id(&[]), Err(InvalidId(0)));
    }

    #[test]
    fn channels_are_named_after_their_user_or_device() {
        let user = Uuid::parse_str("0192f5a0-0000-7000-8000-000000000001").unwrap();
        assert_eq!(
            notify_channel(user),
            "notify:0192f5a0-0000-7000-8000-000000000001"
        );
        assert_eq!(
            kick_channel(user),
            "kick:0192f5a0-0000-7000-8000-000000000001"
        );
    }

    #[test]
    fn a_status_carries_the_client_error() {
        let error = Error {
            code: ErrorCode::NotContact.into(),
            message: "the users are not accepted contacts".to_owned(),
            retryable: false,
            client_msg_id: Some(id_bytes(Uuid::now_v7())),
            request_id: Some(7),
            retry_after_s: None,
        };
        let status = error_status(tonic::Code::FailedPrecondition, error.clone());
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(status.message(), "the users are not accepted contacts");
        assert_eq!(error_from_status(&status), error);
    }

    #[test]
    fn a_status_without_a_client_error_is_a_retryable_unavailable() {
        let error = error_from_status(&tonic::Status::unavailable("connection refused"));
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert!(error.retryable);
    }

    #[test]
    fn the_details_are_a_google_rpc_status_with_the_error_as_any() {
        let error = Error {
            code: ErrorCode::NotMember.into(),
            message: "not a member of this conversation".to_owned(),
            retryable: false,
            client_msg_id: None,
            request_id: None,
            retry_after_s: None,
        };
        let status = error_status(tonic::Code::FailedPrecondition, error.clone());
        let rich = RpcStatus::decode(status.details()).unwrap();
        assert_eq!(rich.code, tonic::Code::FailedPrecondition as i32);
        assert_eq!(rich.message, "not a member of this conversation");
        assert_eq!(rich.details.len(), 1);
        assert_eq!(rich.details[0].type_url, ERROR_TYPE_URL);
        assert_eq!(Error::decode(rich.details[0].value.as_slice()), Ok(error));

        // A bare push.v1.Error, or an Any of another type, is not a client error.
        let bare = tonic::Status::with_details(
            tonic::Code::FailedPrecondition,
            "bare",
            Error {
                code: ErrorCode::NotMember.into(),
                ..Error::default()
            }
            .encode_to_vec()
            .into(),
        );
        assert_eq!(error_from_status(&bare).code(), ErrorCode::Unavailable);
        let other = RpcStatus {
            code: tonic::Code::FailedPrecondition as i32,
            message: "other".to_owned(),
            details: vec![Any {
                type_url: "type.googleapis.com/google.rpc.ErrorInfo".to_owned(),
                value: rich.details[0].value.clone(),
            }],
        };
        let other = tonic::Status::with_details(
            tonic::Code::FailedPrecondition,
            "other",
            other.encode_to_vec().into(),
        );
        assert_eq!(error_from_status(&other).code(), ErrorCode::Unavailable);
    }
}
