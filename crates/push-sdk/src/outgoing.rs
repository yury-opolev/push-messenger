//! Proactive agents (spec 2026-11-09 proactive agents): what an agent starts on its own (a
//! contact request, a new direct chat, a group or an invite to one, a call to someone other
//! than its owner) passes its owner's outgoing policy, and who reaches the agent's backend
//! follows its inbound policy. The agent reads both with `agent_settings` and its actions
//! with `outgoing_actions`; the owner sets them through the owner methods.
//!
//! Times are milliseconds since the Unix epoch.

use push_domain::{ConversationId, UserId};
use push_proto::v1;
use uuid::Uuid;

use crate::error::error_code_name;
use crate::model::redacted;

/// What the owner lets an agent do on its own, for one kind of action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutgoingPolicy {
    /// Carried out at once, within the agent's limits.
    Allowed,
    /// It waits for the owner: the agent gets `waiting_for_owner` with the action's id.
    Ask,
    /// Refused: `not_allowed_by_owner`.
    Off,
}

impl OutgoingPolicy {
    /// The API's name: `allowed`, `ask` or `off`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Ask => "ask",
            Self::Off => "off",
        }
    }

    /// The policy with this API name.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "allowed" => Self::Allowed,
            "ask" => Self::Ask,
            "off" => Self::Off,
            _ => return None,
        })
    }

    pub(crate) fn from_wire(policy: v1::OutgoingPolicy) -> Option<Self> {
        match policy {
            v1::OutgoingPolicy::Allowed => Some(Self::Allowed),
            v1::OutgoingPolicy::Ask => Some(Self::Ask),
            v1::OutgoingPolicy::Off => Some(Self::Off),
            v1::OutgoingPolicy::Unspecified => None,
        }
    }
}

/// Whose messages reach an agent's backend (the bridge enforces it). Everyone else gets a
/// private notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InboundPolicy {
    /// The owner only (the default).
    OwnerOnly,
    /// The owner, and accepted contacts that are agents of the same owner.
    OwnerAgents,
    /// Every accepted contact, people and agents.
    Contacts,
}

impl InboundPolicy {
    /// The API's name: `owner_only`, `owner_agents` or `contacts`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnerOnly => "owner_only",
            Self::OwnerAgents => "owner_agents",
            Self::Contacts => "contacts",
        }
    }

    /// The policy with this API name.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "owner_only" => Self::OwnerOnly,
            "owner_agents" => Self::OwnerAgents,
            "contacts" => Self::Contacts,
            _ => return None,
        })
    }

    pub(crate) fn from_wire(policy: v1::InboundPolicy) -> Option<Self> {
        match policy {
            v1::InboundPolicy::OwnerOnly => Some(Self::OwnerOnly),
            v1::InboundPolicy::OwnerAgents => Some(Self::OwnerAgents),
            v1::InboundPolicy::Contacts => Some(Self::Contacts),
            v1::InboundPolicy::Unspecified => None,
        }
    }
}

/// An agent's settings, set by its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentSettings {
    pub inbound: InboundPolicy,
    /// Contact requests.
    pub requests: OutgoingPolicy,
    /// New direct chats with someone other than the owner.
    pub chats: OutgoingPolicy,
    /// Creating groups and adding members to them.
    pub groups: OutgoingPolicy,
    /// Calls to someone other than the owner.
    pub calls: OutgoingPolicy,
}

impl AgentSettings {
    /// `None` when a policy is missing.
    pub(crate) fn from_wire(settings: &v1::AgentSettings) -> Option<Self> {
        Some(Self {
            inbound: InboundPolicy::from_wire(settings.inbound())?,
            requests: OutgoingPolicy::from_wire(settings.requests())?,
            chats: OutgoingPolicy::from_wire(settings.chats())?,
            groups: OutgoingPolicy::from_wire(settings.groups())?,
            calls: OutgoingPolicy::from_wire(settings.calls())?,
        })
    }
}

/// What an agent started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutgoingKind {
    /// A contact request.
    Request,
    /// A new direct chat.
    Chat,
    /// A new group.
    Group,
    /// Adding members to a group.
    Invite,
    /// A call to someone other than the owner.
    Call,
}

impl OutgoingKind {
    /// The API's name: `request`, `chat`, `group`, `invite` or `call`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Chat => "chat",
            Self::Group => "group",
            Self::Invite => "invite",
            Self::Call => "call",
        }
    }

    /// The kind with this API name.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "request" => Self::Request,
            "chat" => Self::Chat,
            "group" => Self::Group,
            "invite" => Self::Invite,
            "call" => Self::Call,
            _ => return None,
        })
    }

    pub(crate) fn from_wire(kind: v1::OutgoingKind) -> Option<Self> {
        match kind {
            v1::OutgoingKind::Request => Some(Self::Request),
            v1::OutgoingKind::Chat => Some(Self::Chat),
            v1::OutgoingKind::Group => Some(Self::Group),
            v1::OutgoingKind::Invite => Some(Self::Invite),
            v1::OutgoingKind::Call => Some(Self::Call),
            v1::OutgoingKind::Unspecified => None,
        }
    }
}

/// Where an agent's action stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutgoingState {
    /// It waits for the owner.
    Waiting,
    /// The owner approved it and it was carried out (an approved call may ring that person
    /// once within 15 minutes).
    Approved,
    /// The owner declined it. Agents also get it for an action that expired.
    OwnerDeclined,
    /// Nobody decided it within 24 hours. Owners only.
    Expired,
    /// Approved, but the normal rules refused it: the action's `error` says why.
    Failed,
}

impl OutgoingState {
    /// The owner API's name: `waiting`, `approved`, `declined`, `expired` or `failed`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Approved => "approved",
            Self::OwnerDeclined => "declined",
            Self::Expired => "expired",
            Self::Failed => "failed",
        }
    }

    /// The state with this owner API name.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "waiting" => Self::Waiting,
            "approved" => Self::Approved,
            "declined" => Self::OwnerDeclined,
            "expired" => Self::Expired,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    pub(crate) fn from_wire(state: v1::OutgoingState) -> Option<Self> {
        match state {
            v1::OutgoingState::Waiting => Some(Self::Waiting),
            v1::OutgoingState::Approved => Some(Self::Approved),
            v1::OutgoingState::OwnerDeclined => Some(Self::OwnerDeclined),
            v1::OutgoingState::Expired => Some(Self::Expired),
            v1::OutgoingState::Failed => Some(Self::Failed),
            v1::OutgoingState::Unspecified => None,
        }
    }
}

/// An action an agent started, as the agent (or, in an event, its owner) gets it over the
/// connection. `Debug` shows a group's title as its length only.
#[derive(Clone, PartialEq, Eq)]
pub struct OutgoingAction {
    /// The action's id (a UUID), as in `waiting_for_owner` errors.
    pub action_id: String,
    pub agent: UserId,
    pub kind: OutgoingKind,
    /// Request, chat and call: the other user.
    pub target: Option<UserId>,
    /// Invite: the group; once approved, the chat or group it opened or created.
    pub conversation: Option<ConversationId>,
    /// Group: its title.
    pub title: Option<String>,
    /// Group and invite: the members, sorted.
    pub members: Vec<UserId>,
    pub state: OutgoingState,
    /// Failed: why, as an error code (`not_contact`, `blocked`, …).
    pub error: Option<String>,
    pub created_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
}

impl OutgoingAction {
    /// `None` when it names no action, agent, kind or state, or an id is broken.
    pub(crate) fn from_wire(action: &v1::OutgoingAction) -> Option<Self> {
        let optional = |bytes: &[u8]| -> Option<Option<Uuid>> {
            if bytes.is_empty() {
                Some(None)
            } else {
                Uuid::from_slice(bytes).ok().map(Some)
            }
        };
        let error = match action.error() {
            v1::ErrorCode::Unspecified => None,
            code => Some(error_code_name(code)),
        };
        Some(Self {
            action_id: Uuid::from_slice(&action.action_id).ok()?.to_string(),
            agent: UserId(Uuid::from_slice(&action.agent_user_id).ok()?),
            kind: OutgoingKind::from_wire(action.kind())?,
            target: optional(&action.target_user_id)?.map(UserId),
            conversation: optional(&action.conversation_id)?.map(ConversationId),
            title: Some(action.title.clone()).filter(|title| !title.is_empty()),
            members: action
                .member_ids
                .iter()
                .map(|member| Uuid::from_slice(member).ok().map(UserId))
                .collect::<Option<_>>()?,
            state: OutgoingState::from_wire(action.state())?,
            error,
            created_at_ms: action.created_at.div_euclid(1_000),
            resolved_at_ms: Some(action.resolved_at)
                .filter(|micros| *micros != 0)
                .map(|micros| micros.div_euclid(1_000)),
        })
    }
}

impl std::fmt::Debug for OutgoingAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutgoingAction")
            .field("action_id", &self.action_id)
            .field("agent", &self.agent)
            .field("kind", &self.kind)
            .field("target", &self.target)
            .field("conversation", &self.conversation)
            .field("title", &redacted(&self.title))
            .field("members", &self.members)
            .field("state", &self.state)
            .field("error", &self.error)
            .field("created_at_ms", &self.created_at_ms)
            .field("resolved_at_ms", &self.resolved_at_ms)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::bytes;

    #[test]
    fn every_name_reads_back() {
        for policy in [
            OutgoingPolicy::Allowed,
            OutgoingPolicy::Ask,
            OutgoingPolicy::Off,
        ] {
            assert_eq!(OutgoingPolicy::from_name(policy.as_str()), Some(policy));
        }
        for policy in [
            InboundPolicy::OwnerOnly,
            InboundPolicy::OwnerAgents,
            InboundPolicy::Contacts,
        ] {
            assert_eq!(InboundPolicy::from_name(policy.as_str()), Some(policy));
        }
        for kind in [
            OutgoingKind::Request,
            OutgoingKind::Chat,
            OutgoingKind::Group,
            OutgoingKind::Invite,
            OutgoingKind::Call,
        ] {
            assert_eq!(OutgoingKind::from_name(kind.as_str()), Some(kind));
        }
        for state in [
            OutgoingState::Waiting,
            OutgoingState::Approved,
            OutgoingState::OwnerDeclined,
            OutgoingState::Expired,
            OutgoingState::Failed,
        ] {
            assert_eq!(OutgoingState::from_name(state.as_str()), Some(state));
        }
        assert_eq!(OutgoingState::OwnerDeclined.as_str(), "declined");
        assert_eq!(OutgoingPolicy::from_name("maybe"), None);
    }

    #[test]
    fn an_action_reads_from_the_wire_and_hides_its_title_in_debug() {
        let (action, agent, member) = (Uuid::now_v7(), UserId::new(), UserId::new());
        let wire = v1::OutgoingAction {
            action_id: bytes(action),
            agent_user_id: bytes(agent.0),
            kind: v1::OutgoingKind::Group.into(),
            title: "secret plans".to_owned(),
            member_ids: vec![bytes(member.0)],
            state: v1::OutgoingState::Failed.into(),
            error: v1::ErrorCode::NotContact.into(),
            created_at: 1_790_000_000_123_456,
            resolved_at: 1_790_000_001_000_000,
            ..Default::default()
        };
        let read = OutgoingAction::from_wire(&wire).unwrap();
        assert_eq!(
            read,
            OutgoingAction {
                action_id: action.to_string(),
                agent,
                kind: OutgoingKind::Group,
                target: None,
                conversation: None,
                title: Some("secret plans".to_owned()),
                members: vec![member],
                state: OutgoingState::Failed,
                error: Some("not_contact".to_owned()),
                created_at_ms: 1_790_000_000_123,
                resolved_at_ms: Some(1_790_000_001_000),
            }
        );
        assert!(!format!("{read:?}").contains("secret"));
        let unreadable = [
            v1::OutgoingAction {
                action_id: vec![1, 2, 3],
                ..wire.clone()
            },
            v1::OutgoingAction {
                kind: v1::OutgoingKind::Unspecified.into(),
                ..wire.clone()
            },
            v1::OutgoingAction {
                target_user_id: vec![9],
                ..wire.clone()
            },
        ];
        for broken in unreadable {
            assert_eq!(OutgoingAction::from_wire(&broken), None);
        }
    }
}
