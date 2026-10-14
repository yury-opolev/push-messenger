//! Between domain values and protobuf messages.

use chrono::{DateTime, Utc};
use push_domain::{ContactState, ContentType, DomainError, Entry, EntryKind, SendReceipt};
use push_proto::v1::{self, Deliver, ErrorCode, SendAck};
use push_proto::{error_status, id, id_bytes};
use tonic::{Code, Status};
use uuid::Uuid;

/// A time as microseconds since the Unix epoch.
pub(crate) fn micros(at: DateTime<Utc>) -> i64 {
    at.timestamp_micros()
}

/// A time from microseconds since the Unix epoch.
pub(crate) fn from_micros(micros: i64) -> Result<DateTime<Utc>, Status> {
    DateTime::from_timestamp_micros(micros).ok_or_else(|| invalid("a time is out of range"))
}

/// An identifier field, wrapped in its domain type. A field that is not 16 bytes is an
/// `INVALID_ARGUMENT` naming the field.
pub(crate) fn parse_id<T>(bytes: &[u8], field: &str, wrap: fn(Uuid) -> T) -> Result<T, Status> {
    id(bytes)
        .map(wrap)
        .map_err(|error| invalid(format!("{field}: {error}")))
}

/// An `INVALID_ARGUMENT` status for a malformed request.
pub(crate) fn invalid(message: impl Into<String>) -> Status {
    error_status(
        Code::InvalidArgument,
        v1::Error {
            code: ErrorCode::Invalid.into(),
            message: message.into(),
            retryable: false,
            client_msg_id: None,
            request_id: None,
        },
    )
}

/// The status for a failed domain operation: every [`DomainError`] has a gRPC code and a
/// client error code of its own (this match has no catch-all, so a new error must be
/// mapped). Storage problems are reported without their details, which the store has
/// already logged.
pub(crate) fn domain_error(error: DomainError) -> Status {
    let (code, error_code) = match &error {
        DomainError::Invalid(_) => (Code::InvalidArgument, ErrorCode::Invalid),
        DomainError::UnknownUser => (Code::NotFound, ErrorCode::UserNotFound),
        DomainError::UnknownDevice => (Code::PermissionDenied, ErrorCode::UnknownDevice),
        DomainError::DeviceNotKnownYet => (Code::Unavailable, ErrorCode::Unavailable),
        DomainError::ConversationNotFound => (Code::NotFound, ErrorCode::ConversationNotFound),
        DomainError::NotMember => (Code::FailedPrecondition, ErrorCode::NotMember),
        DomainError::NotContact => (Code::FailedPrecondition, ErrorCode::NotContact),
        DomainError::NoPendingRequest => (Code::FailedPrecondition, ErrorCode::NoPendingRequest),
        DomainError::Blocked => (Code::FailedPrecondition, ErrorCode::Blocked),
        DomainError::ClientIdConflict => (Code::AlreadyExists, ErrorCode::ClientIdConflict),
        DomainError::SeqOutOfRange => (Code::OutOfRange, ErrorCode::SeqOutOfRange),
        DomainError::TargetNotFound => (Code::NotFound, ErrorCode::TargetNotFound),
        DomainError::NotSender => (Code::PermissionDenied, ErrorCode::NotSender),
        DomainError::AlreadyDeleted => (Code::FailedPrecondition, ErrorCode::AlreadyDeleted),
        DomainError::WindowExpired => (Code::FailedPrecondition, ErrorCode::WindowExpired),
        DomainError::InviteNotFound => (Code::NotFound, ErrorCode::InviteNotFound),
        DomainError::GroupFull => (Code::FailedPrecondition, ErrorCode::GroupFull),
        DomainError::NotAGroup => (Code::FailedPrecondition, ErrorCode::NotAGroup),
        DomainError::NotAdmin => (Code::PermissionDenied, ErrorCode::NotAdmin),
        DomainError::LastAdmin => (Code::FailedPrecondition, ErrorCode::LastAdmin),
        DomainError::Conflict => (Code::Aborted, ErrorCode::Unavailable),
        DomainError::Store(_) => (Code::Unavailable, ErrorCode::Unavailable),
    };
    let message = match &error {
        DomainError::Store(_) => "a temporary storage problem; retry".to_owned(),
        other => other.to_string(),
    };
    error_status(
        code,
        v1::Error {
            code: error_code.into(),
            message,
            retryable: error.is_retryable() || matches!(error, DomainError::Store(_)),
            client_msg_id: None,
            request_id: None,
        },
    )
}

/// A log entry as the client receives it, in its current state.
pub(crate) fn deliver(entry: Entry) -> Deliver {
    Deliver {
        conversation_id: id_bytes(entry.conversation_id.0),
        seq: entry.seq,
        message_id: id_bytes(entry.message_id.0),
        client_msg_id: id_bytes(entry.client_msg_id.0),
        kind: entry_kind(entry.kind).into(),
        target_seq: entry.target_seq,
        sender_id: id_bytes(entry.sender_id.0),
        content_type: content_type(entry.content_type).into(),
        body: entry.body,
        client_ts: entry.client_ts.map(micros),
        server_ts: micros(entry.server_ts),
        edited_at: entry.edited_at.map(micros),
        edit_seq: entry.edit_seq,
        deleted: entry.deleted_at.is_some(),
    }
}

/// What the sender learns once its message is stored.
pub(crate) fn send_ack(receipt: &SendReceipt) -> SendAck {
    SendAck {
        client_msg_id: id_bytes(receipt.client_msg_id.0),
        message_id: id_bytes(receipt.message_id.0),
        seq: receipt.seq,
        server_ts: micros(receipt.server_ts),
    }
}

fn entry_kind(kind: EntryKind) -> v1::EntryKind {
    match kind {
        EntryKind::Message => v1::EntryKind::Message,
        EntryKind::Edit => v1::EntryKind::Edit,
        EntryKind::Delete => v1::EntryKind::Delete,
        EntryKind::System => v1::EntryKind::System,
    }
}

fn content_type(content_type: ContentType) -> v1::ContentType {
    match content_type {
        ContentType::Text => v1::ContentType::Text,
        ContentType::SystemEvent => v1::ContentType::SystemEvent,
    }
}

/// A user's view of a relationship; `None` is no relationship.
pub(crate) fn contact_state(state: Option<ContactState>) -> v1::ContactState {
    match state {
        None => v1::ContactState::None,
        Some(ContactState::RequestedOut) => v1::ContactState::RequestedOut,
        Some(ContactState::RequestedIn) => v1::ContactState::RequestedIn,
        Some(ContactState::Accepted) => v1::ContactState::Accepted,
        Some(ContactState::Blocked) => v1::ContactState::Blocked,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use push_domain::{ClientMsgId, ConversationId, DeviceId, MessageId, UserId};
    use push_proto::error_from_status;

    use super::*;

    #[test]
    fn times_travel_as_microseconds() {
        let at = Utc.with_ymd_and_hms(2026, 10, 12, 9, 30, 0).unwrap()
            + chrono::TimeDelta::microseconds(123_456);
        assert_eq!(from_micros(micros(at)).unwrap(), at);
        assert_eq!(
            from_micros(i64::MAX).unwrap_err().code(),
            Code::InvalidArgument
        );
    }

    #[test]
    fn an_id_field_of_the_wrong_length_is_invalid() {
        let user = Uuid::now_v7();
        assert_eq!(
            parse_id(&id_bytes(user), "user_id", UserId).unwrap(),
            UserId(user)
        );
        let status = parse_id(&[1, 2, 3], "user_id", UserId).unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
        let error = error_from_status(&status);
        assert_eq!(error.code(), ErrorCode::Invalid);
        assert_eq!(error.message, "user_id: an id must be 16 bytes, not 3");
    }

    #[test]
    fn an_edit_is_delivered_with_its_target_seq() {
        let at = Utc.with_ymd_and_hms(2026, 10, 12, 9, 30, 0).unwrap();
        let entry = Entry {
            conversation_id: ConversationId::new(),
            seq: 7,
            message_id: MessageId::new(),
            client_msg_id: ClientMsgId::new(),
            kind: EntryKind::Edit,
            target_seq: Some(5),
            sender_id: UserId::new(),
            sender_device_id: Some(DeviceId::new()),
            content_type: ContentType::Text,
            body: Some(b"fixed".to_vec()),
            client_ts: Some(at),
            server_ts: at,
            edited_at: None,
            edit_seq: None,
            deleted_at: None,
        };
        let deliver = deliver(entry.clone());
        assert_eq!(deliver.conversation_id, id_bytes(entry.conversation_id.0));
        assert_eq!(deliver.seq, 7);
        assert_eq!(deliver.message_id, id_bytes(entry.message_id.0));
        assert_eq!(deliver.client_msg_id, id_bytes(entry.client_msg_id.0));
        assert_eq!(deliver.kind(), v1::EntryKind::Edit);
        assert_eq!(deliver.target_seq, Some(5));
        assert_eq!(deliver.sender_id, id_bytes(entry.sender_id.0));
        assert_eq!(deliver.content_type(), v1::ContentType::Text);
        assert_eq!(deliver.body, Some(b"fixed".to_vec()));
        assert_eq!(deliver.client_ts, Some(micros(at)));
        assert_eq!(deliver.server_ts, micros(at));
        assert!(!deliver.deleted);
    }

    #[test]
    fn a_deleted_message_is_delivered_without_a_body() {
        let at = Utc.with_ymd_and_hms(2026, 10, 12, 9, 30, 0).unwrap();
        let entry = Entry {
            conversation_id: ConversationId::new(),
            seq: 5,
            message_id: MessageId::new(),
            client_msg_id: ClientMsgId::new(),
            kind: EntryKind::Message,
            target_seq: None,
            sender_id: UserId::new(),
            sender_device_id: Some(DeviceId::new()),
            content_type: ContentType::Text,
            body: None,
            client_ts: None,
            server_ts: at,
            edited_at: Some(at),
            edit_seq: Some(6),
            deleted_at: Some(at),
        };
        let deliver = deliver(entry);
        assert_eq!(deliver.body, None);
        assert!(deliver.deleted);
        assert_eq!(deliver.edited_at, Some(micros(at)));
        assert_eq!(deliver.edit_seq, Some(6));
    }

    #[test]
    fn every_domain_error_has_its_own_status_and_client_error() {
        use push_domain::ValidationError;

        let cases = [
            (
                DomainError::Invalid(ValidationError::EmptyBody),
                Code::InvalidArgument,
                ErrorCode::Invalid,
            ),
            (
                DomainError::UnknownUser,
                Code::NotFound,
                ErrorCode::UserNotFound,
            ),
            (
                DomainError::UnknownDevice,
                Code::PermissionDenied,
                ErrorCode::UnknownDevice,
            ),
            (
                DomainError::DeviceNotKnownYet,
                Code::Unavailable,
                ErrorCode::Unavailable,
            ),
            (
                DomainError::ConversationNotFound,
                Code::NotFound,
                ErrorCode::ConversationNotFound,
            ),
            (
                DomainError::NotMember,
                Code::FailedPrecondition,
                ErrorCode::NotMember,
            ),
            (
                DomainError::NotContact,
                Code::FailedPrecondition,
                ErrorCode::NotContact,
            ),
            (
                DomainError::NoPendingRequest,
                Code::FailedPrecondition,
                ErrorCode::NoPendingRequest,
            ),
            (
                DomainError::Blocked,
                Code::FailedPrecondition,
                ErrorCode::Blocked,
            ),
            (
                DomainError::ClientIdConflict,
                Code::AlreadyExists,
                ErrorCode::ClientIdConflict,
            ),
            (
                DomainError::SeqOutOfRange,
                Code::OutOfRange,
                ErrorCode::SeqOutOfRange,
            ),
            (
                DomainError::TargetNotFound,
                Code::NotFound,
                ErrorCode::TargetNotFound,
            ),
            (
                DomainError::NotSender,
                Code::PermissionDenied,
                ErrorCode::NotSender,
            ),
            (
                DomainError::AlreadyDeleted,
                Code::FailedPrecondition,
                ErrorCode::AlreadyDeleted,
            ),
            (
                DomainError::WindowExpired,
                Code::FailedPrecondition,
                ErrorCode::WindowExpired,
            ),
            (
                DomainError::InviteNotFound,
                Code::NotFound,
                ErrorCode::InviteNotFound,
            ),
            (
                DomainError::GroupFull,
                Code::FailedPrecondition,
                ErrorCode::GroupFull,
            ),
            (
                DomainError::NotAGroup,
                Code::FailedPrecondition,
                ErrorCode::NotAGroup,
            ),
            (
                DomainError::NotAdmin,
                Code::PermissionDenied,
                ErrorCode::NotAdmin,
            ),
            (
                DomainError::LastAdmin,
                Code::FailedPrecondition,
                ErrorCode::LastAdmin,
            ),
            (DomainError::Conflict, Code::Aborted, ErrorCode::Unavailable),
            (
                DomainError::Store("relation missing".into()),
                Code::Unavailable,
                ErrorCode::Unavailable,
            ),
        ];
        let mut client_codes = std::collections::HashSet::new();
        for (domain, code, error_code) in cases {
            let retryable = matches!(
                domain,
                DomainError::DeviceNotKnownYet | DomainError::Conflict | DomainError::Store(_)
            );
            let message = domain.to_string();
            let status = domain_error(domain);
            assert_eq!(status.code(), code, "{message}");
            let error = error_from_status(&status);
            assert_eq!(error.code(), error_code, "{message}");
            assert_eq!(error.retryable, retryable, "{message}");
            client_codes.insert(error_code);
        }
        // Only the retryable errors share a client code.
        assert_eq!(client_codes.len(), 20);

        let store = error_from_status(&domain_error(DomainError::Store("relation".into())));
        assert!(!store.message.contains("relation"), "{}", store.message);
        let contact = error_from_status(&domain_error(DomainError::NotContact));
        assert_eq!(contact.message, "the users are not accepted contacts");
    }
}
