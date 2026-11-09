//! Between domain values and protobuf messages.

use chrono::{DateTime, Utc};
use push_domain::{
    ActivityState, AgentSettings, ContactState, ContentType, DomainError, Entry, EntryKind,
    InboundPolicy, OutgoingAction, OutgoingError, OutgoingKind, OutgoingPolicy, OutgoingState,
    PresenceView, SendReceipt, UserId, ValidationError,
};
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
            retry_after_s: None,
            action_id: None,
        },
    )
}

/// The status for a failed domain operation: every [`DomainError`] has a gRPC code and a
/// client error code (this match has no catch-all, so a new error must be mapped). Storage
/// problems are reported without their details, which the store has already logged.
pub(crate) fn domain_error(error: DomainError) -> Status {
    let (code, error_code) = match &error {
        DomainError::Invalid(_) => (Code::InvalidArgument, ErrorCode::Invalid),
        DomainError::UnknownUser => (Code::NotFound, ErrorCode::UserNotFound),
        DomainError::UnknownDevice => (Code::PermissionDenied, ErrorCode::UnknownDevice),
        DomainError::DeviceNotKnownYet => (Code::Unavailable, ErrorCode::DeviceNotReady),
        DomainError::PasswordChanged => (Code::Unauthenticated, ErrorCode::PasswordChanged),
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
        DomainError::Template(_) => (Code::InvalidArgument, ErrorCode::Invalid),
        DomainError::CardAnswered => (Code::FailedPrecondition, ErrorCode::Rejected),
        DomainError::TemplateNotFound => (Code::NotFound, ErrorCode::Rejected),
        DomainError::TemplateExists => (Code::AlreadyExists, ErrorCode::Rejected),
        DomainError::TemplateConflict(_) => (Code::FailedPrecondition, ErrorCode::Rejected),
        DomainError::NotAllowed => (Code::FailedPrecondition, ErrorCode::NotAllowed),
        DomainError::OwnerDecides => (Code::FailedPrecondition, ErrorCode::OwnerDecides),
        DomainError::RateLimited { .. } => (Code::ResourceExhausted, ErrorCode::RateLimited),
        DomainError::AgentNotFound => (Code::NotFound, ErrorCode::UserNotFound),
        DomainError::WaitingForOwner { .. } => {
            (Code::FailedPrecondition, ErrorCode::WaitingForOwner)
        }
        DomainError::NotAllowedByOwner => (Code::FailedPrecondition, ErrorCode::NotAllowedByOwner),
        DomainError::OutgoingNotFound => (Code::NotFound, ErrorCode::TargetNotFound),
        DomainError::Conflict => (Code::Aborted, ErrorCode::Unavailable),
        DomainError::KickFailed(_) => (Code::Unavailable, ErrorCode::Unavailable),
        DomainError::Store(_) => (Code::Unavailable, ErrorCode::Unavailable),
    };
    let message = match &error {
        DomainError::Store(_) => "a temporary storage problem; retry".to_owned(),
        DomainError::KickFailed(_) => "the change was saved but not delivered; retry".to_owned(),
        other => other.to_string(),
    };
    error_status(
        code,
        v1::Error {
            code: error_code.into(),
            message,
            retryable: error.is_retryable()
                || matches!(
                    error,
                    DomainError::Store(_) | DomainError::RateLimited { .. }
                ),
            client_msg_id: None,
            request_id: None,
            retry_after_s: match error {
                DomainError::RateLimited { retry_after_s } => Some(retry_after_s),
                _ => None,
            },
            action_id: match error {
                DomainError::WaitingForOwner { action_id } => Some(id_bytes(action_id)),
                _ => None,
            },
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
        answer_seq: entry.answer_seq,
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
        ContentType::Rich => v1::ContentType::Rich,
        ContentType::Answer => v1::ContentType::Answer,
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

/// An activity state as the client sends it; unspecified is `Invalid`.
pub(crate) fn activity_state_from_wire(state: v1::ActivityState) -> Result<ActivityState, Status> {
    match state {
        v1::ActivityState::Typing => Ok(ActivityState::Typing),
        v1::ActivityState::Working => Ok(ActivityState::Working),
        v1::ActivityState::Waiting => Ok(ActivityState::Waiting),
        v1::ActivityState::Idle => Ok(ActivityState::Idle),
        v1::ActivityState::Unspecified => Err(domain_error(ValidationError::ActivityState.into())),
    }
}

/// An activity state as the members receive it.
pub(crate) fn activity_state_to_wire(state: ActivityState) -> v1::ActivityState {
    match state {
        ActivityState::Typing => v1::ActivityState::Typing,
        ActivityState::Working => v1::ActivityState::Working,
        ActivityState::Waiting => v1::ActivityState::Waiting,
        ActivityState::Idle => v1::ActivityState::Idle,
    }
}

/// `user`'s presence as a viewer is shown it: `last_seen` only with offline.
pub(crate) fn presence(user: UserId, view: PresenceView) -> v1::Presence {
    let (state, last_seen) = match view {
        PresenceView::Unknown => (v1::PresenceState::Unknown, None),
        PresenceView::Online => (v1::PresenceState::Online, None),
        PresenceView::Offline { last_seen } => (v1::PresenceState::Offline, last_seen.map(micros)),
    };
    v1::Presence {
        user_id: id_bytes(user.0),
        state: state.into(),
        last_seen,
    }
}

pub(crate) fn outgoing_policy(policy: OutgoingPolicy) -> v1::OutgoingPolicy {
    match policy {
        OutgoingPolicy::Allowed => v1::OutgoingPolicy::Allowed,
        OutgoingPolicy::Ask => v1::OutgoingPolicy::Ask,
        OutgoingPolicy::Off => v1::OutgoingPolicy::Off,
    }
}

/// An outgoing policy as the owner sends it; unspecified or unknown is `INVALID_ARGUMENT`
/// naming `field`.
pub(crate) fn outgoing_policy_from_wire(code: i32, field: &str) -> Result<OutgoingPolicy, Status> {
    match v1::OutgoingPolicy::try_from(code) {
        Ok(v1::OutgoingPolicy::Allowed) => Ok(OutgoingPolicy::Allowed),
        Ok(v1::OutgoingPolicy::Ask) => Ok(OutgoingPolicy::Ask),
        Ok(v1::OutgoingPolicy::Off) => Ok(OutgoingPolicy::Off),
        Ok(v1::OutgoingPolicy::Unspecified) | Err(_) => {
            Err(invalid(format!("{field}: must be allowed, ask or off")))
        }
    }
}

pub(crate) fn inbound_policy(policy: InboundPolicy) -> v1::InboundPolicy {
    match policy {
        InboundPolicy::OwnerOnly => v1::InboundPolicy::OwnerOnly,
        InboundPolicy::OwnerAgents => v1::InboundPolicy::OwnerAgents,
        InboundPolicy::Contacts => v1::InboundPolicy::Contacts,
    }
}

/// An inbound policy as the owner sends it; unspecified or unknown is `INVALID_ARGUMENT`.
pub(crate) fn inbound_policy_from_wire(code: i32) -> Result<InboundPolicy, Status> {
    match v1::InboundPolicy::try_from(code) {
        Ok(v1::InboundPolicy::OwnerOnly) => Ok(InboundPolicy::OwnerOnly),
        Ok(v1::InboundPolicy::OwnerAgents) => Ok(InboundPolicy::OwnerAgents),
        Ok(v1::InboundPolicy::Contacts) => Ok(InboundPolicy::Contacts),
        Ok(v1::InboundPolicy::Unspecified) | Err(_) => Err(invalid(
            "policy: must be owner_only, owner_agents or contacts",
        )),
    }
}

pub(crate) fn agent_settings(settings: &AgentSettings) -> v1::AgentSettings {
    v1::AgentSettings {
        inbound: inbound_policy(settings.inbound).into(),
        requests: outgoing_policy(settings.requests).into(),
        chats: outgoing_policy(settings.chats).into(),
        groups: outgoing_policy(settings.groups).into(),
        calls: outgoing_policy(settings.calls).into(),
    }
}

/// Who an [`OutgoingAction`] is shown to: the agent does not tell an expired action from
/// a declined one (spec 2026-11-09 proactive agents D7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Viewer {
    Owner,
    Agent,
}

/// An agent's action as `viewer` sees it.
pub(crate) fn outgoing_action(action: &OutgoingAction, viewer: Viewer) -> v1::OutgoingAction {
    let state = match action.state {
        OutgoingState::Waiting => v1::OutgoingState::Waiting,
        OutgoingState::Approved => v1::OutgoingState::Approved,
        OutgoingState::Declined => v1::OutgoingState::OwnerDeclined,
        OutgoingState::Expired => match viewer {
            Viewer::Owner => v1::OutgoingState::Expired,
            Viewer::Agent => v1::OutgoingState::OwnerDeclined,
        },
        OutgoingState::Failed => v1::OutgoingState::Failed,
    };
    let kind = match action.kind {
        OutgoingKind::Request => v1::OutgoingKind::Request,
        OutgoingKind::Chat => v1::OutgoingKind::Chat,
        OutgoingKind::Group => v1::OutgoingKind::Group,
        OutgoingKind::Invite => v1::OutgoingKind::Invite,
        OutgoingKind::Call => v1::OutgoingKind::Call,
    };
    v1::OutgoingAction {
        action_id: id_bytes(action.action_id),
        agent_user_id: id_bytes(action.agent.0),
        kind: kind.into(),
        target_user_id: action
            .target
            .map(|user| id_bytes(user.0))
            .unwrap_or_default(),
        conversation_id: action
            .conversation
            .map(|conversation| id_bytes(conversation.0))
            .unwrap_or_default(),
        title: action.title.clone().unwrap_or_default(),
        member_ids: action
            .members
            .iter()
            .map(|member| id_bytes(member.0))
            .collect(),
        state: state.into(),
        error: action
            .error
            .map_or(ErrorCode::Unspecified, outgoing_error)
            .into(),
        created_at: micros(action.created_at),
        resolved_at: action.resolved_at.map(micros).unwrap_or_default(),
    }
}

/// The client code of the error an approved action failed with.
fn outgoing_error(error: OutgoingError) -> ErrorCode {
    match error {
        OutgoingError::NotContact => ErrorCode::NotContact,
        OutgoingError::Blocked => ErrorCode::Blocked,
        OutgoingError::NotAdmin => ErrorCode::NotAdmin,
        OutgoingError::NotMember => ErrorCode::NotMember,
        OutgoingError::GroupFull => ErrorCode::GroupFull,
        OutgoingError::NotAGroup => ErrorCode::NotAGroup,
        OutgoingError::ConversationNotFound => ErrorCode::ConversationNotFound,
        OutgoingError::NotAllowed => ErrorCode::NotAllowed,
        OutgoingError::RateLimited => ErrorCode::RateLimited,
        OutgoingError::Invalid => ErrorCode::Invalid,
        OutgoingError::Failed => ErrorCode::Rejected,
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
            answer_seq: None,
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
            answer_seq: None,
        };
        let deliver = deliver(entry);
        assert_eq!(deliver.body, None);
        assert!(deliver.deleted);
        assert_eq!(deliver.edited_at, Some(micros(at)));
        assert_eq!(deliver.edit_seq, Some(6));
    }

    #[test]
    fn an_answered_card_is_delivered_as_rich_with_its_answer_seq() {
        let at = Utc.with_ymd_and_hms(2026, 10, 26, 9, 30, 0).unwrap();
        let card = Entry {
            conversation_id: ConversationId::new(),
            seq: 4,
            message_id: MessageId::new(),
            client_msg_id: ClientMsgId::new(),
            kind: EntryKind::Message,
            target_seq: None,
            sender_id: UserId::new(),
            sender_device_id: None,
            content_type: ContentType::Rich,
            body: Some(b"{}".to_vec()),
            client_ts: None,
            server_ts: at,
            edited_at: None,
            edit_seq: None,
            deleted_at: None,
            answer_seq: Some(9),
        };
        let answer = Entry {
            seq: 9,
            target_seq: Some(4),
            content_type: ContentType::Answer,
            answer_seq: None,
            ..card.clone()
        };
        let card = deliver(card);
        assert_eq!(card.content_type(), v1::ContentType::Rich);
        assert_eq!(card.answer_seq, Some(9));
        let answer = deliver(answer);
        assert_eq!(answer.content_type(), v1::ContentType::Answer);
        assert_eq!(answer.target_seq, Some(4));
        assert_eq!(answer.answer_seq, None);
    }

    #[test]
    fn every_domain_error_has_a_status_and_a_client_error() {
        use push_domain::ValidationError;
        use push_domain::templates::TemplateError;

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
                ErrorCode::DeviceNotReady,
            ),
            (
                DomainError::PasswordChanged,
                Code::Unauthenticated,
                ErrorCode::PasswordChanged,
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
            (
                DomainError::Template(TemplateError("unknown block".into())),
                Code::InvalidArgument,
                ErrorCode::Invalid,
            ),
            (
                DomainError::CardAnswered,
                Code::FailedPrecondition,
                ErrorCode::Rejected,
            ),
            (
                DomainError::TemplateNotFound,
                Code::NotFound,
                ErrorCode::Rejected,
            ),
            (
                DomainError::TemplateExists,
                Code::AlreadyExists,
                ErrorCode::Rejected,
            ),
            (
                DomainError::TemplateConflict("the template is revoked"),
                Code::FailedPrecondition,
                ErrorCode::Rejected,
            ),
            (
                DomainError::NotAllowed,
                Code::FailedPrecondition,
                ErrorCode::NotAllowed,
            ),
            (
                DomainError::OwnerDecides,
                Code::FailedPrecondition,
                ErrorCode::OwnerDecides,
            ),
            (
                DomainError::RateLimited { retry_after_s: 42 },
                Code::ResourceExhausted,
                ErrorCode::RateLimited,
            ),
            (
                DomainError::AgentNotFound,
                Code::NotFound,
                ErrorCode::UserNotFound,
            ),
            (
                DomainError::WaitingForOwner {
                    action_id: Uuid::nil(),
                },
                Code::FailedPrecondition,
                ErrorCode::WaitingForOwner,
            ),
            (
                DomainError::NotAllowedByOwner,
                Code::FailedPrecondition,
                ErrorCode::NotAllowedByOwner,
            ),
            (
                DomainError::OutgoingNotFound,
                Code::NotFound,
                ErrorCode::TargetNotFound,
            ),
            (DomainError::Conflict, Code::Aborted, ErrorCode::Unavailable),
            (
                DomainError::KickFailed("redis down".into()),
                Code::Unavailable,
                ErrorCode::Unavailable,
            ),
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
                DomainError::DeviceNotKnownYet
                    | DomainError::Conflict
                    | DomainError::KickFailed(_)
                    | DomainError::Store(_)
                    | DomainError::RateLimited { .. }
            );
            let message = domain.to_string();
            let status = domain_error(domain);
            assert_eq!(status.code(), code, "{message}");
            let error = error_from_status(&status);
            assert_eq!(error.code(), error_code, "{message}");
            assert_eq!(error.retryable, retryable, "{message}");
            client_codes.insert(error_code);
        }
        // Only the retryable errors, the template errors, an unknown agent (as an unknown
        // user) and an action that does not wait (as a missing target) share a client code.
        assert_eq!(client_codes.len(), 28);
        let limited = error_from_status(&domain_error(DomainError::RateLimited {
            retry_after_s: 42,
        }));
        assert_eq!(limited.retry_after_s, Some(42));
        assert_eq!(limited.action_id, None);
        let action_id = Uuid::now_v7();
        let waiting = error_from_status(&domain_error(DomainError::WaitingForOwner { action_id }));
        assert_eq!(waiting.action_id, Some(id_bytes(action_id)));
        assert!(!waiting.retryable);

        let store = error_from_status(&domain_error(DomainError::Store("relation".into())));
        assert!(!store.message.contains("relation"), "{}", store.message);
        let contact = error_from_status(&domain_error(DomainError::NotContact));
        assert_eq!(contact.message, "the users are not accepted contacts");
    }
}
