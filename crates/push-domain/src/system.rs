//! System entries: membership and settings changes recorded in a group's log (spec §6).
//!
//! A system entry has `kind = System` and `content_type = SystemEvent`, and its body is
//! the event as compact JSON with a format version and a type tag:
//!
//! ```text
//! {"v":1,"type":"members_added","actor":"0190…","users":["0190…","0190…"]}
//! ```
//!
//! JSON, because every client language decodes it without generated code, a client can
//! show an unknown `type` as a generic line, and fields can be added without a new
//! version. The largest body, a group created with 99 other members, is about 4 KB.

use serde::{Deserialize, Serialize};

use crate::ids::UserId;
use crate::model::Role;

/// The format version written into every system entry body.
pub const SYSTEM_EVENT_VERSION: u32 = 1;

/// What a system entry records. `actor` is the user whose action caused it; it is also
/// the entry's `sender_id`.
///
/// Decoders must tolerate new event types; `decode` returns `None` for types this version
/// does not know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SystemEvent {
    /// `actor` created the group titled `title` with `users` as its other first members.
    Created {
        actor: UserId,
        title: String,
        users: Vec<UserId>,
    },
    /// `actor` added `users`.
    MembersAdded { actor: UserId, users: Vec<UserId> },
    /// `actor` removed `user`.
    MemberRemoved { actor: UserId, user: UserId },
    /// `actor` left.
    MemberLeft { actor: UserId },
    /// `actor` renamed the group to `title`.
    Renamed { actor: UserId, title: String },
    /// `actor` gave `user` the role `role`.
    RoleChanged {
        actor: UserId,
        user: UserId,
        role: Role,
    },
    /// No admin remained after `actor` left, so `user` became an admin automatically.
    AdminPromoted { actor: UserId, user: UserId },
}

#[derive(Serialize, Deserialize)]
struct Envelope<E> {
    v: u32,
    #[serde(flatten)]
    event: E,
}

impl SystemEvent {
    /// The user whose action caused the event.
    pub fn actor(&self) -> UserId {
        match self {
            Self::Created { actor, .. }
            | Self::MembersAdded { actor, .. }
            | Self::MemberRemoved { actor, .. }
            | Self::MemberLeft { actor }
            | Self::Renamed { actor, .. }
            | Self::RoleChanged { actor, .. }
            | Self::AdminPromoted { actor, .. } => *actor,
        }
    }

    /// The entry body: compact JSON.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(&Envelope {
            v: SYSTEM_EVENT_VERSION,
            event: self,
        })
        .expect("a system event always serialises")
    }

    /// Reads an entry body. `None` for another format version, an unknown type or a body
    /// that is not a system event.
    pub fn decode(body: &[u8]) -> Option<Self> {
        let envelope: Envelope<Self> = serde_json::from_slice(body).ok()?;
        (envelope.v == SYSTEM_EVENT_VERSION).then_some(envelope.event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn user(n: u128) -> UserId {
        UserId(Uuid::from_u128(n))
    }

    #[test]
    fn the_body_is_compact_json_with_a_version_and_a_type() {
        let event = SystemEvent::RoleChanged {
            actor: user(1),
            user: user(2),
            role: Role::Admin,
        };
        assert_eq!(
            String::from_utf8(event.encode()).unwrap(),
            concat!(
                r#"{"v":1,"type":"role_changed","#,
                r#""actor":"00000000-0000-0000-0000-000000000001","#,
                r#""user":"00000000-0000-0000-0000-000000000002","role":"admin"}"#
            )
        );
    }

    #[test]
    fn every_event_survives_a_round_trip() {
        let events = [
            SystemEvent::Created {
                actor: user(1),
                title: "Trip \"2026\"".into(),
                users: vec![user(2), user(3)],
            },
            SystemEvent::MembersAdded {
                actor: user(1),
                users: vec![user(4)],
            },
            SystemEvent::MemberRemoved {
                actor: user(1),
                user: user(4),
            },
            SystemEvent::MemberLeft { actor: user(2) },
            SystemEvent::Renamed {
                actor: user(1),
                title: "Trip".into(),
            },
            SystemEvent::RoleChanged {
                actor: user(1),
                user: user(3),
                role: Role::Member,
            },
            SystemEvent::AdminPromoted {
                actor: user(1),
                user: user(3),
            },
        ];
        for event in events {
            assert_eq!(SystemEvent::decode(&event.encode()), Some(event));
        }
    }

    #[test]
    fn every_variant_has_a_pinned_encoding() {
        let (a, b, c) = (user(1), user(2), user(3));
        let cases = [
            (
                SystemEvent::Created {
                    actor: a,
                    title: "T".into(),
                    users: vec![b, c],
                },
                concat!(
                    r#"{"v":1,"type":"created","actor":"00000000-0000-0000-0000-000000000001","#,
                    r#""title":"T","users":["00000000-0000-0000-0000-000000000002","#,
                    r#""00000000-0000-0000-0000-000000000003"]}"#
                ),
            ),
            (
                SystemEvent::MembersAdded {
                    actor: a,
                    users: vec![b],
                },
                concat!(
                    r#"{"v":1,"type":"members_added","actor":"00000000-0000-0000-0000-000000000001","#,
                    r#""users":["00000000-0000-0000-0000-000000000002"]}"#
                ),
            ),
            (
                SystemEvent::MemberRemoved { actor: a, user: b },
                concat!(
                    r#"{"v":1,"type":"member_removed","actor":"00000000-0000-0000-0000-000000000001","#,
                    r#""user":"00000000-0000-0000-0000-000000000002"}"#
                ),
            ),
            (
                SystemEvent::MemberLeft { actor: a },
                r#"{"v":1,"type":"member_left","actor":"00000000-0000-0000-0000-000000000001"}"#,
            ),
            (
                SystemEvent::Renamed {
                    actor: a,
                    title: "T".into(),
                },
                concat!(
                    r#"{"v":1,"type":"renamed","actor":"00000000-0000-0000-0000-000000000001","#,
                    r#""title":"T"}"#
                ),
            ),
            (
                SystemEvent::RoleChanged {
                    actor: a,
                    user: b,
                    role: Role::Member,
                },
                concat!(
                    r#"{"v":1,"type":"role_changed","actor":"00000000-0000-0000-0000-000000000001","#,
                    r#""user":"00000000-0000-0000-0000-000000000002","role":"member"}"#
                ),
            ),
            (
                SystemEvent::AdminPromoted { actor: a, user: b },
                concat!(
                    r#"{"v":1,"type":"admin_promoted","actor":"00000000-0000-0000-0000-000000000001","#,
                    r#""user":"00000000-0000-0000-0000-000000000002"}"#
                ),
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(String::from_utf8(event.encode()).unwrap(), expected);
        }
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let body = r#"{"v":1,"type":"member_left","actor":"00000000-0000-0000-0000-000000000001","extra":1}"#;
        assert_eq!(
            SystemEvent::decode(body.as_bytes()),
            Some(SystemEvent::MemberLeft { actor: user(1) })
        );
    }

    #[test]
    fn other_versions_types_and_bodies_are_not_decoded() {
        let actor = r#""actor":"00000000-0000-0000-0000-000000000001""#;
        for body in [
            format!(r#"{{"v":2,"type":"member_left",{actor}}}"#),
            format!(r#"{{"v":1,"type":"pinned",{actor}}}"#),
            "hello".to_string(),
        ] {
            assert_eq!(SystemEvent::decode(body.as_bytes()), None);
        }
    }
}
