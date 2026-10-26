//! The smallint codes the schema stores for domain enumerations.

use push_domain::{ContactState, ContentType, ConversationKind, EntryKind, Role, TemplateStatus};

/// A domain enumeration stored as a `smallint`.
pub(crate) trait Code: Sized + Copy + 'static {
    /// Every value with its code.
    const CODES: &'static [(Self, i16)];
    /// The column holding it, for error messages.
    const COLUMN: &'static str;
}

impl Code for ContactState {
    const CODES: &'static [(Self, i16)] = &[
        (Self::RequestedOut, 1),
        (Self::RequestedIn, 2),
        (Self::Accepted, 3),
        (Self::Blocked, 4),
    ];
    const COLUMN: &'static str = "contact.state";
}

impl Code for ConversationKind {
    const CODES: &'static [(Self, i16)] = &[(Self::Direct, 1), (Self::Group, 2)];
    const COLUMN: &'static str = "conversation.kind";
}

impl Code for Role {
    const CODES: &'static [(Self, i16)] = &[(Self::Admin, 1), (Self::Member, 2)];
    const COLUMN: &'static str = "conversation_member.role";
}

impl Code for EntryKind {
    const CODES: &'static [(Self, i16)] = &[
        (Self::Message, 1),
        (Self::Edit, 2),
        (Self::Delete, 3),
        (Self::System, 4),
    ];
    const COLUMN: &'static str = "message.kind";
}

impl Code for ContentType {
    const CODES: &'static [(Self, i16)] = &[
        (Self::Text, 1),
        (Self::SystemEvent, 2),
        (Self::Rich, 3),
        (Self::Answer, 4),
    ];
    const COLUMN: &'static str = "message.content_type";
}

impl Code for TemplateStatus {
    const CODES: &'static [(Self, i16)] = &[
        (Self::Pending, 1),
        (Self::Approved, 2),
        (Self::Rejected, 3),
        (Self::Revoked, 4),
    ];
    const COLUMN: &'static str = "template_version.status";
}

/// The code stored for `value`.
pub(crate) fn encode<T: Code + PartialEq>(value: T) -> i16 {
    T::CODES
        .iter()
        .find(|(known, _)| *known == value)
        .map(|(_, code)| *code)
        .expect("every value has a code")
}

/// The value stored as `code`; a decode error for a code the schema does not allow.
pub(crate) fn decode<T: Code>(code: i16) -> Result<T, sqlx::Error> {
    T::CODES
        .iter()
        .find(|(_, known)| *known == code)
        .map(|(value, _)| *value)
        .ok_or_else(|| sqlx::Error::Decode(format!("unknown {} code {code}", T::COLUMN).into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trips<T: Code + PartialEq + std::fmt::Debug>() {
        for (value, code) in T::CODES {
            assert_eq!(encode(*value), *code);
            assert_eq!(decode::<T>(*code).unwrap(), *value);
        }
        assert!(decode::<T>(0).is_err());
    }

    #[test]
    fn every_code_round_trips_and_unknown_codes_are_refused() {
        round_trips::<ContactState>();
        round_trips::<ConversationKind>();
        round_trips::<Role>();
        round_trips::<EntryKind>();
        round_trips::<ContentType>();
        round_trips::<TemplateStatus>();
    }

    #[test]
    fn codes_match_the_schema_comments() {
        assert_eq!(encode(ContactState::Blocked), 4);
        assert_eq!(encode(ConversationKind::Group), 2);
        assert_eq!(encode(Role::Admin), 1);
        assert_eq!(encode(EntryKind::System), 4);
        assert_eq!(encode(ContentType::SystemEvent), 2);
        assert_eq!(encode(ContentType::Answer), 4);
        assert_eq!(encode(TemplateStatus::Revoked), 4);
    }
}
