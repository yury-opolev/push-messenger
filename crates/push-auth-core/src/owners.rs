//! An agent's owner acting on the agent (spec 2026-11-06 agent connections and the owner
//! view, §5.1): `/v1/agents/{agent_id}/…`. auth-svc checks the ownership itself, in one
//! query, before anything else: everyone but the owner gets `not_found`, the same answer
//! as for an agent that does not exist. message-svc's `AgentOwners` API (with auth-svc's
//! internal token, like `Directory`) holds the policy and the relationships and checks the
//! ownership again.

use async_trait::async_trait;
use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, TimeDelta, Utc};
use push_proto::internal::v1::agent_owners_client::AgentOwnersClient;
use push_proto::internal::v1::{
    self as internal, AgentCallHistoryRequest, AgentHistoryRequest, AgentPeerRequest, AgentRef,
    ResolveAction, ResolveOutgoingRequest, ResolveRequest, SetAgentPolicyRequest,
    SetInboundRequest, SetOutgoingRequest,
};
use push_proto::v1::{
    self as v1, ContactState, ContentType, ConversationKind, Deliver, EntryKind, ErrorCode,
    HistoryPage, InboundPolicy, OutgoingKind, OutgoingPolicy, OutgoingState,
};
use push_proto::{error_from_status, id, id_bytes};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::api::{Api, ClientIp, JsonBody, caller, profile_json};
use crate::audit::OwnerAudit;
use crate::calls::{CallCursor, CallEntry};
use crate::directory::{MessageSvc, message_svc};
use crate::error::AuthError;
use crate::limits::Action;
use crate::profiles::PublicProfile;
use crate::rules::Invalid;
use crate::service::Auth;
use crate::templates::rfc3339;

/// The policy names of the API, in the order of their codes.
pub const POLICIES: [&str; 5] = [
    "ask",
    "auto_own",
    "auto_owner_contacts",
    "auto_any",
    "refuse_all",
];

/// What the owner does with a waiting request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolve {
    Approve,
    Decline,
    Block,
}

/// The outgoing kinds of the API (spec 2026-11-09 proactive agents, D1), in the order of
/// [`AgentSettings::outgoing`].
pub const OUTGOING_KINDS: [&str; 4] = ["requests", "chats", "groups", "calls"];

/// The values of an outgoing policy.
pub const OUTGOING_POLICIES: [&str; 3] = ["allowed", "ask", "off"];

/// The values of the inbound policy (D8).
pub const INBOUND_POLICIES: [&str; 3] = ["owner_only", "owner_agents", "contacts"];

/// An agent's settings as the API names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSettings {
    /// One of [`INBOUND_POLICIES`].
    pub inbound: &'static str,
    /// One of [`OUTGOING_POLICIES`] per kind of [`OUTGOING_KINDS`].
    pub outgoing: [&'static str; 4],
}

impl Default for AgentSettings {
    /// What an agent whose owner never changed anything has.
    fn default() -> Self {
        Self {
            inbound: "owner_only",
            outgoing: ["ask", "ask", "off", "off"],
        }
    }
}

/// An action an agent started (D4), as the owner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingAction {
    pub action_id: Uuid,
    /// `request`, `chat`, `group`, `invite` or `call`.
    pub kind: &'static str,
    /// Request, chat and call: the other user.
    pub target: Option<Uuid>,
    /// Invite: the group; once approved, the chat or group it opened or created.
    pub conversation: Option<Uuid>,
    /// Group: its title.
    pub title: Option<String>,
    /// Group and invite: the members.
    pub members: Vec<Uuid>,
    /// `waiting`, `approved`, `declined`, `expired` or `failed`.
    pub state: &'static str,
    /// Failed: why, as the snake case name of the error code (`not_contact`).
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

/// A waiting action ends `expired` this long after it was created.
const ACTION_EXPIRY: TimeDelta = TimeDelta::hours(24);

/// message-svc's `AgentOwners` API. Every method is for `owner`'s `agent`; message-svc
/// answers `NotFound` for any other pair.
#[async_trait]
pub trait AgentOwnerApi: Send + Sync + std::fmt::Debug {
    /// The agent's policy, one of [`POLICIES`].
    async fn policy(&self, owner: Uuid, agent: Uuid) -> Result<&'static str, AuthError>;
    /// Sets the policy (one of [`POLICIES`]); the policy now.
    async fn set_policy(
        &self,
        owner: Uuid,
        agent: Uuid,
        policy: &str,
    ) -> Result<&'static str, AuthError>;
    /// The waiting requests, oldest first.
    async fn waiting(
        &self,
        owner: Uuid,
        agent: Uuid,
    ) -> Result<Vec<(Uuid, DateTime<Utc>)>, AuthError>;
    /// Approves, declines or blocks `user`. No waiting request to approve or decline is
    /// `NotFound`; the owner is `OwnerContact`.
    async fn resolve(
        &self,
        owner: Uuid,
        agent: Uuid,
        user: Uuid,
        action: Resolve,
    ) -> Result<(), AuthError>;
    /// The agent's relationships as `(user, state)`, state one of `requested_out`,
    /// `requested_in`, `accepted`, `blocked`.
    async fn contacts(
        &self,
        owner: Uuid,
        agent: Uuid,
    ) -> Result<Vec<(Uuid, &'static str)>, AuthError>;
    /// Ends the agent's relationship with `user`; the owner is `OwnerContact`.
    async fn remove(&self, owner: Uuid, agent: Uuid, user: Uuid) -> Result<(), AuthError>;
    /// The agent's conversations, newest activity first (read only).
    async fn conversations(
        &self,
        owner: Uuid,
        agent: Uuid,
    ) -> Result<Vec<internal::AgentConversation>, AuthError>;
    /// A page of one of the agent's conversations as the agent sees it (read only); a
    /// conversation the agent is not in is `NotFound`.
    async fn history(
        &self,
        owner: Uuid,
        agent: Uuid,
        conversation: Uuid,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<HistoryPage, AuthError>;
    /// The agent's call history, newest first (read only).
    async fn calls(
        &self,
        owner: Uuid,
        agent: Uuid,
        before: Option<CallCursor>,
        limit: u32,
    ) -> Result<Vec<CallEntry>, AuthError>;
    /// The agent's settings.
    async fn settings(&self, owner: Uuid, agent: Uuid) -> Result<AgentSettings, AuthError>;
    /// Changes the outgoing policies given (each one of [`OUTGOING_POLICIES`], in the order
    /// of [`OUTGOING_KINDS`]); the settings now.
    async fn set_outgoing(
        &self,
        owner: Uuid,
        agent: Uuid,
        change: [Option<&'static str>; 4],
    ) -> Result<AgentSettings, AuthError>;
    /// Sets the inbound policy (one of [`INBOUND_POLICIES`]); the settings now.
    async fn set_inbound(
        &self,
        owner: Uuid,
        agent: Uuid,
        policy: &'static str,
    ) -> Result<AgentSettings, AuthError>;
    /// The agent's waiting actions, oldest first.
    async fn outgoing(&self, owner: Uuid, agent: Uuid) -> Result<Vec<OutgoingAction>, AuthError>;
    /// Approves (carries out as the agent) or declines a waiting action; the action now. One
    /// that is not waiting is `NotFound`.
    async fn resolve_outgoing(
        &self,
        owner: Uuid,
        agent: Uuid,
        action: Uuid,
        approve: bool,
    ) -> Result<OutgoingAction, AuthError>;
}

/// message-svc's `AgentOwners` API over gRPC.
#[derive(Debug, Clone)]
pub struct GrpcAgentOwners {
    client: AgentOwnersClient<MessageSvc>,
}

impl GrpcAgentOwners {
    /// A client for message-svc at `url` presenting `token` (auth-svc's internal token).
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        Ok(Self::over(message_svc(url, token)?))
    }

    pub(crate) fn over(channel: MessageSvc) -> Self {
        Self {
            client: AgentOwnersClient::new(channel),
        }
    }
}

fn of(owner: Uuid, agent: Uuid) -> AgentRef {
    AgentRef {
        owner_user_id: id_bytes(owner),
        agent_user_id: id_bytes(agent),
    }
}

/// message-svc's refusal as an answer: an unknown pair and a missing request are
/// `NotFound`, the owner as the target `OwnerContact`; anything else (logged with its
/// codes only) `Unavailable`.
pub(crate) fn refused(rpc: &'static str, status: &tonic::Status) -> AuthError {
    let error = error_from_status(status);
    match (status.code(), error.code()) {
        (tonic::Code::NotFound, _)
        | (
            _,
            ErrorCode::NoPendingRequest | ErrorCode::NotMember | ErrorCode::ConversationNotFound,
        ) => AuthError::NotFound,
        (_, ErrorCode::NotAllowed) => AuthError::OwnerContact,
        (tonic::Code::InvalidArgument, _) => AuthError::Invalid(Invalid {
            field: "user_id",
            reason: "not a user this agent can have a relationship with",
        }),
        (code, client) => {
            tracing::warn!(
                ?code,
                ?client,
                rpc,
                "message-svc's AgentOwners API did not answer"
            );
            AuthError::Unavailable
        }
    }
}

fn policy_name(policy: internal::AgentPolicy) -> Result<&'static str, AuthError> {
    let index = match policy {
        internal::AgentPolicy::Ask => 0,
        internal::AgentPolicy::AutoOwn => 1,
        internal::AgentPolicy::AutoOwnerContacts => 2,
        internal::AgentPolicy::AutoAny => 3,
        internal::AgentPolicy::RefuseAll => 4,
        internal::AgentPolicy::Unspecified => {
            tracing::warn!("message-svc answered an unspecified agent policy");
            return Err(AuthError::Unavailable);
        }
    };
    Ok(POLICIES[index])
}

fn policy_code(name: &str) -> Option<internal::AgentPolicy> {
    Some(match name {
        "ask" => internal::AgentPolicy::Ask,
        "auto_own" => internal::AgentPolicy::AutoOwn,
        "auto_owner_contacts" => internal::AgentPolicy::AutoOwnerContacts,
        "auto_any" => internal::AgentPolicy::AutoAny,
        "refuse_all" => internal::AgentPolicy::RefuseAll,
        _ => return None,
    })
}

fn state_name(state: ContactState) -> &'static str {
    match state {
        ContactState::RequestedOut => "requested_out",
        ContactState::RequestedIn => "requested_in",
        ContactState::Accepted => "accepted",
        ContactState::Blocked => "blocked",
        ContactState::None => "none",
    }
}

fn malformed(what: &str) -> AuthError {
    tracing::warn!(
        what,
        "message-svc's AgentOwners API answered with a malformed value"
    );
    AuthError::Unavailable
}

pub(crate) fn user(bytes: &[u8]) -> Result<Uuid, AuthError> {
    id(bytes).map_err(|_| malformed("a user id"))
}

/// An optional id: empty bytes are none.
fn optional_id(bytes: &[u8]) -> Result<Option<Uuid>, AuthError> {
    if bytes.is_empty() {
        Ok(None)
    } else {
        user(bytes).map(Some)
    }
}

fn outgoing_name(policy: OutgoingPolicy) -> Result<&'static str, AuthError> {
    Ok(match policy {
        OutgoingPolicy::Allowed => "allowed",
        OutgoingPolicy::Ask => "ask",
        OutgoingPolicy::Off => "off",
        OutgoingPolicy::Unspecified => return Err(malformed("an outgoing policy")),
    })
}

fn outgoing_code(name: &str) -> Option<OutgoingPolicy> {
    Some(match name {
        "allowed" => OutgoingPolicy::Allowed,
        "ask" => OutgoingPolicy::Ask,
        "off" => OutgoingPolicy::Off,
        _ => return None,
    })
}

fn inbound_name(policy: InboundPolicy) -> Result<&'static str, AuthError> {
    Ok(match policy {
        InboundPolicy::OwnerOnly => "owner_only",
        InboundPolicy::OwnerAgents => "owner_agents",
        InboundPolicy::Contacts => "contacts",
        InboundPolicy::Unspecified => return Err(malformed("an inbound policy")),
    })
}

fn inbound_code(name: &str) -> Option<InboundPolicy> {
    Some(match name {
        "owner_only" => InboundPolicy::OwnerOnly,
        "owner_agents" => InboundPolicy::OwnerAgents,
        "contacts" => InboundPolicy::Contacts,
        _ => return None,
    })
}

fn settings_of(reply: internal::AgentSettingsReply) -> Result<AgentSettings, AuthError> {
    let settings = reply
        .settings
        .ok_or_else(|| malformed("missing settings"))?;
    Ok(AgentSettings {
        inbound: inbound_name(settings.inbound())?,
        outgoing: [
            outgoing_name(settings.requests())?,
            outgoing_name(settings.chats())?,
            outgoing_name(settings.groups())?,
            outgoing_name(settings.calls())?,
        ],
    })
}

/// An error code as the API names it: `ERROR_CODE_NOT_CONTACT` is `not_contact`.
fn error_name(code: ErrorCode) -> Option<String> {
    if code == ErrorCode::Unspecified {
        return None;
    }
    let name = code.as_str_name();
    Some(
        name.strip_prefix("ERROR_CODE_")
            .unwrap_or(name)
            .to_ascii_lowercase(),
    )
}

fn action_of(action: &v1::OutgoingAction) -> Result<OutgoingAction, AuthError> {
    let time =
        |micros: i64| DateTime::from_timestamp_micros(micros).ok_or_else(|| malformed("a time"));
    let kind = match action.kind() {
        OutgoingKind::Request => "request",
        OutgoingKind::Chat => "chat",
        OutgoingKind::Group => "group",
        OutgoingKind::Invite => "invite",
        OutgoingKind::Call => "call",
        OutgoingKind::Unspecified => return Err(malformed("an action kind")),
    };
    let state = match action.state() {
        OutgoingState::Waiting => "waiting",
        OutgoingState::Approved => "approved",
        OutgoingState::OwnerDeclined => "declined",
        OutgoingState::Expired => "expired",
        OutgoingState::Failed => "failed",
        OutgoingState::Unspecified => return Err(malformed("an action state")),
    };
    Ok(OutgoingAction {
        action_id: id(&action.action_id).map_err(|_| malformed("an action id"))?,
        kind,
        target: optional_id(&action.target_user_id)?,
        conversation: optional_id(&action.conversation_id)?,
        title: (!action.title.is_empty()).then(|| action.title.clone()),
        members: action
            .member_ids
            .iter()
            .map(|member| user(member))
            .collect::<Result<_, _>>()?,
        state,
        error: error_name(action.error()),
        created_at: time(action.created_at)?,
        resolved_at: (action.resolved_at != 0)
            .then(|| time(action.resolved_at))
            .transpose()?,
    })
}

#[async_trait]
impl AgentOwnerApi for GrpcAgentOwners {
    async fn policy(&self, owner: Uuid, agent: Uuid) -> Result<&'static str, AuthError> {
        let reply = self
            .client
            .clone()
            .get_policy(of(owner, agent))
            .await
            .map_err(|status| refused("GetPolicy", &status))?;
        policy_name(reply.into_inner().policy())
    }

    async fn set_policy(
        &self,
        owner: Uuid,
        agent: Uuid,
        policy: &str,
    ) -> Result<&'static str, AuthError> {
        let code = policy_code(policy).ok_or(AuthError::Invalid(Invalid {
            field: "policy",
            reason: "expected ask, auto_own, auto_owner_contacts, auto_any or refuse_all",
        }))?;
        let reply = self
            .client
            .clone()
            .set_policy(SetAgentPolicyRequest {
                agent: Some(of(owner, agent)),
                policy: code.into(),
            })
            .await
            .map_err(|status| refused("SetPolicy", &status))?;
        policy_name(reply.into_inner().policy())
    }

    async fn waiting(
        &self,
        owner: Uuid,
        agent: Uuid,
    ) -> Result<Vec<(Uuid, DateTime<Utc>)>, AuthError> {
        let reply = self
            .client
            .clone()
            .list_waiting(of(owner, agent))
            .await
            .map_err(|status| refused("ListWaiting", &status))?;
        reply
            .into_inner()
            .requests
            .iter()
            .map(|request| {
                let at = DateTime::from_timestamp_micros(request.requested_at)
                    .ok_or_else(|| malformed("a time"))?;
                Ok((user(&request.user_id)?, at))
            })
            .collect()
    }

    async fn resolve(
        &self,
        owner: Uuid,
        agent: Uuid,
        user: Uuid,
        action: Resolve,
    ) -> Result<(), AuthError> {
        let action = match action {
            Resolve::Approve => ResolveAction::Approve,
            Resolve::Decline => ResolveAction::Decline,
            Resolve::Block => ResolveAction::Block,
        };
        self.client
            .clone()
            .resolve(ResolveRequest {
                agent: Some(of(owner, agent)),
                user_id: id_bytes(user),
                action: action.into(),
            })
            .await
            .map_err(|status| refused("Resolve", &status))?;
        Ok(())
    }

    async fn contacts(
        &self,
        owner: Uuid,
        agent: Uuid,
    ) -> Result<Vec<(Uuid, &'static str)>, AuthError> {
        let reply = self
            .client
            .clone()
            .list_contacts(of(owner, agent))
            .await
            .map_err(|status| refused("ListContacts", &status))?;
        reply
            .into_inner()
            .contacts
            .iter()
            .map(|contact| Ok((user(&contact.user_id)?, state_name(contact.state()))))
            .collect()
    }

    async fn remove(&self, owner: Uuid, agent: Uuid, user: Uuid) -> Result<(), AuthError> {
        self.client
            .clone()
            .remove_contact(AgentPeerRequest {
                agent: Some(of(owner, agent)),
                user_id: id_bytes(user),
            })
            .await
            .map_err(|status| refused("RemoveContact", &status))?;
        Ok(())
    }

    async fn conversations(
        &self,
        owner: Uuid,
        agent: Uuid,
    ) -> Result<Vec<internal::AgentConversation>, AuthError> {
        let reply = self
            .client
            .clone()
            .conversations(of(owner, agent))
            .await
            .map_err(|status| refused("Conversations", &status))?;
        Ok(reply.into_inner().conversations)
    }

    async fn history(
        &self,
        owner: Uuid,
        agent: Uuid,
        conversation: Uuid,
        before_seq: Option<i64>,
        limit: u32,
    ) -> Result<HistoryPage, AuthError> {
        let reply = self
            .client
            .clone()
            .history(AgentHistoryRequest {
                agent: Some(of(owner, agent)),
                conversation_id: id_bytes(conversation),
                before_seq,
                limit,
            })
            .await
            .map_err(|status| refused("History", &status))?;
        Ok(reply.into_inner())
    }

    async fn calls(
        &self,
        owner: Uuid,
        agent: Uuid,
        before: Option<CallCursor>,
        limit: u32,
    ) -> Result<Vec<CallEntry>, AuthError> {
        let reply = self
            .client
            .clone()
            .call_history(AgentCallHistoryRequest {
                agent: Some(of(owner, agent)),
                before: before.map(CallCursor::to_proto),
                limit,
            })
            .await
            .map_err(|status| refused("CallHistory", &status))?;
        reply
            .into_inner()
            .calls
            .iter()
            .map(crate::calls::entry)
            .collect()
    }

    async fn settings(&self, owner: Uuid, agent: Uuid) -> Result<AgentSettings, AuthError> {
        let reply = self
            .client
            .clone()
            .get_settings(of(owner, agent))
            .await
            .map_err(|status| refused("GetSettings", &status))?;
        settings_of(reply.into_inner())
    }

    async fn set_outgoing(
        &self,
        owner: Uuid,
        agent: Uuid,
        change: [Option<&'static str>; 4],
    ) -> Result<AgentSettings, AuthError> {
        let mut codes = [None; 4];
        for ((code, name), kind) in codes.iter_mut().zip(change).zip(OUTGOING_KINDS) {
            if let Some(name) = name {
                let policy = outgoing_code(name).ok_or(AuthError::Invalid(Invalid {
                    field: kind,
                    reason: "expected allowed, ask or off",
                }))?;
                *code = Some(i32::from(policy));
            }
        }
        let [requests, chats, groups, calls] = codes;
        let reply = self
            .client
            .clone()
            .set_outgoing(SetOutgoingRequest {
                agent: Some(of(owner, agent)),
                requests,
                chats,
                groups,
                calls,
            })
            .await
            .map_err(|status| refused("SetOutgoing", &status))?;
        settings_of(reply.into_inner())
    }

    async fn set_inbound(
        &self,
        owner: Uuid,
        agent: Uuid,
        policy: &'static str,
    ) -> Result<AgentSettings, AuthError> {
        let code = inbound_code(policy).ok_or(AuthError::Invalid(Invalid {
            field: "policy",
            reason: "expected owner_only, owner_agents or contacts",
        }))?;
        let reply = self
            .client
            .clone()
            .set_inbound(SetInboundRequest {
                agent: Some(of(owner, agent)),
                policy: code.into(),
            })
            .await
            .map_err(|status| refused("SetInbound", &status))?;
        settings_of(reply.into_inner())
    }

    async fn outgoing(&self, owner: Uuid, agent: Uuid) -> Result<Vec<OutgoingAction>, AuthError> {
        let reply = self
            .client
            .clone()
            .list_outgoing(of(owner, agent))
            .await
            .map_err(|status| refused("ListOutgoing", &status))?;
        reply.into_inner().actions.iter().map(action_of).collect()
    }

    async fn resolve_outgoing(
        &self,
        owner: Uuid,
        agent: Uuid,
        action: Uuid,
        approve: bool,
    ) -> Result<OutgoingAction, AuthError> {
        let reply = self
            .client
            .clone()
            .resolve_outgoing(ResolveOutgoingRequest {
                agent: Some(of(owner, agent)),
                action_id: id_bytes(action),
                approve,
            })
            .await
            .map_err(|status| refused("ResolveOutgoing", &status))?;
        let action = reply
            .into_inner()
            .action
            .ok_or_else(|| malformed("missing action"))?;
        action_of(&action)
    }
}

/// Without message-svc: every call is `Unavailable`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAgentOwners;

#[async_trait]
impl AgentOwnerApi for NoAgentOwners {
    async fn policy(&self, _: Uuid, _: Uuid) -> Result<&'static str, AuthError> {
        Err(AuthError::Unavailable)
    }
    async fn set_policy(&self, _: Uuid, _: Uuid, _: &str) -> Result<&'static str, AuthError> {
        Err(AuthError::Unavailable)
    }
    async fn waiting(&self, _: Uuid, _: Uuid) -> Result<Vec<(Uuid, DateTime<Utc>)>, AuthError> {
        Err(AuthError::Unavailable)
    }
    async fn resolve(&self, _: Uuid, _: Uuid, _: Uuid, _: Resolve) -> Result<(), AuthError> {
        Err(AuthError::Unavailable)
    }
    async fn contacts(&self, _: Uuid, _: Uuid) -> Result<Vec<(Uuid, &'static str)>, AuthError> {
        Err(AuthError::Unavailable)
    }
    async fn remove(&self, _: Uuid, _: Uuid, _: Uuid) -> Result<(), AuthError> {
        Err(AuthError::Unavailable)
    }
    async fn conversations(
        &self,
        _: Uuid,
        _: Uuid,
    ) -> Result<Vec<internal::AgentConversation>, AuthError> {
        Err(AuthError::Unavailable)
    }
    async fn history(
        &self,
        _: Uuid,
        _: Uuid,
        _: Uuid,
        _: Option<i64>,
        _: u32,
    ) -> Result<HistoryPage, AuthError> {
        Err(AuthError::Unavailable)
    }
    async fn calls(
        &self,
        _: Uuid,
        _: Uuid,
        _: Option<CallCursor>,
        _: u32,
    ) -> Result<Vec<CallEntry>, AuthError> {
        Err(AuthError::Unavailable)
    }
    async fn settings(&self, _: Uuid, _: Uuid) -> Result<AgentSettings, AuthError> {
        Err(AuthError::Unavailable)
    }
    async fn set_outgoing(
        &self,
        _: Uuid,
        _: Uuid,
        _: [Option<&'static str>; 4],
    ) -> Result<AgentSettings, AuthError> {
        Err(AuthError::Unavailable)
    }
    async fn set_inbound(
        &self,
        _: Uuid,
        _: Uuid,
        _: &'static str,
    ) -> Result<AgentSettings, AuthError> {
        Err(AuthError::Unavailable)
    }
    async fn outgoing(&self, _: Uuid, _: Uuid) -> Result<Vec<OutgoingAction>, AuthError> {
        Err(AuthError::Unavailable)
    }
    async fn resolve_outgoing(
        &self,
        _: Uuid,
        _: Uuid,
        _: Uuid,
        _: bool,
    ) -> Result<OutgoingAction, AuthError> {
        Err(AuthError::Unavailable)
    }
}

impl Auth {
    /// Whether `agent` is an enabled agent account owned by `owner`: one indexed query,
    /// whatever the answer.
    pub async fn owns_agent(&self, owner: Uuid, agent: Uuid) -> Result<bool, AuthError> {
        let owned: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM auth.user_account WHERE user_id = $1 \
             AND owner_user_id = $2 AND kind = 'agent' AND disabled_at IS NULL)",
        )
        .bind(agent)
        .bind(owner)
        .fetch_one(&self.pool)
        .await?;
        Ok(owned)
    }
}

/// The owner and the agent of an owner route, after the rate limit and the ownership
/// check; every refusal but a bad token is `NotFound`. Fills `audit` as it learns.
pub(crate) async fn owner_of(
    api: &Api,
    headers: &HeaderMap,
    ip: Option<std::net::IpAddr>,
    agent_id: &str,
    audit: &mut OwnerAudit,
) -> Result<(Uuid, Uuid), AuthError> {
    let caller = caller(api, headers).await?;
    audit.owner = Some(caller.user_id);
    api.limits
        .check(Action::AgentOwner, ip, Some(&caller.user_id.to_string()))
        .await?;
    let agent: Uuid = agent_id.parse().map_err(|_| AuthError::NotFound)?;
    // Logged on a refusal too, so probing shows which ids were tried.
    audit.agent = Some(agent);
    if !api.auth.owns_agent(caller.user_id, agent).await? {
        return Err(AuthError::NotFound);
    }
    Ok((caller.user_id, agent))
}

/// A user id in a path; anything else is `NotFound`.
pub(crate) fn path_user(text: &str) -> Result<Uuid, AuthError> {
    text.parse().map_err(|_| AuthError::NotFound)
}

/// The answer, carrying what the audit line needs.
pub(crate) fn finish(result: Result<Response, AuthError>, audit: OwnerAudit) -> Response {
    let mut response = result.unwrap_or_else(IntoResponse::into_response);
    response.extensions_mut().insert(audit);
    response
}

/// The profiles of `users` as `agent` may see them (message-svc's `Directory`, as for the
/// agent's own `GET /v1/users`), in the order of `users`: the owner learns nothing about a
/// user the agent itself could not see. A user the agent may not see is shown by id only
/// (`state: "hidden"`); a visible user without a verified account as a deleted account.
pub(crate) async fn profiles_of(
    api: &Api,
    agent: Uuid,
    users: &[Uuid],
) -> Result<Vec<Value>, AuthError> {
    let mut sorted = users.to_vec();
    sorted.sort();
    sorted.dedup();
    let mut visible = api.directory.visible_users(agent, &sorted).await?;
    visible.retain(|user| sorted.binary_search(user).is_ok());
    let found = api.auth.profiles(&visible).await?;
    Ok(users
        .iter()
        .map(|user| {
            if !visible.contains(user) {
                return hidden_profile(*user);
            }
            let profile = found
                .iter()
                .find(|profile| profile.user_id == *user)
                .cloned()
                .unwrap_or_else(|| PublicProfile::tombstone(*user, "user".to_owned()));
            profile_json(&profile)
        })
        .collect())
}

/// A user the agent may not see: the id alone, in the profile's shape.
fn hidden_profile(user: Uuid) -> Value {
    json!({
        "user_id": user,
        "handle": "",
        "display_name": "",
        "kind": "user",
        "owner_user_id": null,
        "owner_display_name": null,
        "owner_handle": null,
        "deleted": false,
        "state": "hidden",
    })
}

/// A peer agent's owner as its profile names them (the notice data), or null.
fn owner_of_peer(peer: &Value) -> Value {
    if peer["owner_user_id"].is_null() {
        return Value::Null;
    }
    json!({
        "user_id": peer["owner_user_id"],
        "handle": peer["owner_handle"].as_str().unwrap_or_default(),
        "display_name": peer["owner_display_name"].as_str().unwrap_or_default(),
        "kind": "user",
        "owner_user_id": null,
        "owner_display_name": null,
        "owner_handle": null,
        "deleted": false,
        "state": "active",
    })
}

/// `GET /v1/agents/{agent_id}/policy`: `{"policy": "ask"}`.
pub(crate) async fn get_policy(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(agent_id): Path<String>,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let policy = api.agent_owners.policy(owner, agent).await?;
        audit.policy = Some(policy);
        Ok(Json(json!({ "policy": policy })).into_response())
    }
    .await;
    finish(result, audit)
}

#[derive(Deserialize)]
pub(crate) struct PolicyBody {
    policy: String,
}

/// `PUT /v1/agents/{agent_id}/policy {"policy"}`: the policy now. Requests already waiting
/// keep waiting.
pub(crate) async fn put_policy(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(agent_id): Path<String>,
    body: Result<JsonBody<PolicyBody>, AuthError>,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let JsonBody(body) = body?;
        let policy = api
            .agent_owners
            .set_policy(owner, agent, &body.policy)
            .await?;
        audit.policy = Some(policy);
        Ok(Json(json!({ "policy": policy })).into_response())
    }
    .await;
    finish(result, audit)
}

/// `GET /v1/agents/{agent_id}/requests`: `{"requests": [{"user": <profile>,
/// "requested_at"}]}`, oldest first.
pub(crate) async fn list_requests(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(agent_id): Path<String>,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let waiting = api.agent_owners.waiting(owner, agent).await?;
        let users: Vec<Uuid> = waiting.iter().map(|(user, _)| *user).collect();
        let profiles = profiles_of(&api, agent, &users).await?;
        audit.count = Some(waiting.len());
        let requests: Vec<Value> = waiting
            .iter()
            .zip(profiles)
            .map(|((_, at), profile)| json!({ "user": profile, "requested_at": rfc3339(*at) }))
            .collect();
        Ok(Json(json!({ "requests": requests })).into_response())
    }
    .await;
    finish(result, audit)
}

async fn resolve(
    api: Api,
    headers: HeaderMap,
    ip: Option<std::net::IpAddr>,
    (agent_id, user_id): (String, String),
    action: Resolve,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let user = path_user(&user_id)?;
        audit.peer = Some(user);
        api.agent_owners.resolve(owner, agent, user, action).await?;
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await;
    finish(result, audit)
}

/// `POST /v1/agents/{agent_id}/requests/{user_id}/approve`: 204.
pub(crate) async fn approve(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(ids): Path<(String, String)>,
) -> Response {
    resolve(api, headers, ip, ids, Resolve::Approve).await
}

/// `POST /v1/agents/{agent_id}/requests/{user_id}/decline`: 204.
pub(crate) async fn decline(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(ids): Path<(String, String)>,
) -> Response {
    resolve(api, headers, ip, ids, Resolve::Decline).await
}

/// `POST /v1/agents/{agent_id}/requests/{user_id}/block`: 204.
pub(crate) async fn block(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(ids): Path<(String, String)>,
) -> Response {
    resolve(api, headers, ip, ids, Resolve::Block).await
}

/// `GET /v1/agents/{agent_id}/contacts`: `{"contacts": [{"user": <profile>, "state",
/// "owner"}]}`.
pub(crate) async fn list_contacts(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(agent_id): Path<String>,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let contacts = api.agent_owners.contacts(owner, agent).await?;
        let users: Vec<Uuid> = contacts.iter().map(|(user, _)| *user).collect();
        let profiles = profiles_of(&api, agent, &users).await?;
        audit.count = Some(contacts.len());
        let contacts: Vec<Value> = contacts
            .iter()
            .zip(profiles)
            .map(|((user, state), profile)| {
                json!({ "user": profile, "state": state, "owner": *user == owner })
            })
            .collect();
        Ok(Json(json!({ "contacts": contacts })).into_response())
    }
    .await;
    finish(result, audit)
}

/// `DELETE /v1/agents/{agent_id}/contacts/{user_id}`: 204, also when there was nothing to
/// remove; the owner is `400 owner_contact`.
pub(crate) async fn remove_contact(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path((agent_id, user_id)): Path<(String, String)>,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let user = path_user(&user_id)?;
        audit.peer = Some(user);
        api.agent_owners.remove(owner, agent, user).await?;
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await;
    finish(result, audit)
}

/// A time in microseconds as the API writes times.
fn time(micros: i64) -> Result<String, AuthError> {
    DateTime::from_timestamp_micros(micros)
        .map(rfc3339)
        .ok_or_else(|| malformed("a time"))
}

/// One history entry as the owner view shows it: exactly what the agent's client gets,
/// with the body as text (every body is UTF-8: text, a card, an answer or a system event).
fn entry_json(entry: &Deliver) -> Result<Value, AuthError> {
    let kind = match entry.kind() {
        EntryKind::Message => "message",
        EntryKind::System => "system",
        EntryKind::Edit => "edit",
        EntryKind::Delete => "delete",
        EntryKind::Unspecified => return Err(malformed("an entry kind")),
    };
    let content_type = match entry.content_type() {
        ContentType::Text => "text",
        ContentType::Rich => "rich",
        ContentType::Answer => "answer",
        ContentType::SystemEvent => "system",
        ContentType::Unspecified => return Err(malformed("a content type")),
    };
    let body = match &entry.body {
        Some(body) => Some(
            String::from_utf8(body.clone()).map_err(|_| malformed("a body that is not UTF-8"))?,
        ),
        None => None,
    };
    Ok(json!({
        "seq": entry.seq,
        "message_id": user(&entry.message_id)?,
        "client_msg_id": user(&entry.client_msg_id)?,
        "kind": kind,
        "sender_id": user(&entry.sender_id)?,
        "content_type": content_type,
        "body": body,
        "deleted": entry.deleted,
        "target_seq": entry.target_seq,
        "answer_seq": entry.answer_seq,
        "client_ts": entry.client_ts.map(time).transpose()?,
        "server_ts": time(entry.server_ts)?,
        "edited_at": entry.edited_at.map(time).transpose()?,
        "edit_seq": entry.edit_seq,
    }))
}

/// `GET /v1/agents/{agent_id}/conversations`: the agent's conversations, newest activity
/// first, with the peer (direct) and the peer's owner (when the peer is an agent) as
/// profiles. Read only.
pub(crate) async fn conversations(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(agent_id): Path<String>,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let conversations = api.agent_owners.conversations(owner, agent).await?;
        let mut users = Vec::new();
        for conversation in &conversations {
            if let Some(id) = &conversation.peer_user_id {
                users.push(user(id)?);
            }
        }
        let profiles = profiles_of(&api, agent, &users).await?;
        let profile = |id: &Option<Vec<u8>>| -> Result<Value, AuthError> {
            match id {
                None => Ok(Value::Null),
                Some(id) => {
                    let id = user(id)?;
                    let at = users.iter().position(|user| *user == id);
                    Ok(at.map_or(Value::Null, |at| profiles[at].clone()))
                }
            }
        };
        let mut listed = Vec::with_capacity(conversations.len());
        for conversation in &conversations {
            let kind = match conversation.kind() {
                ConversationKind::Direct => "direct",
                ConversationKind::Group => "group",
                ConversationKind::Unspecified => return Err(malformed("a conversation kind")),
            };
            let peer = profile(&conversation.peer_user_id)?;
            listed.push(json!({
                "conversation_id": user(&conversation.conversation_id)?,
                "kind": kind,
                "title": conversation.title,
                "peer": peer,
                "peer_owner": owner_of_peer(&peer),
                "member_count": conversation.member_count,
                "last_activity_at": time(conversation.last_activity_at)?,
                "message_count": conversation.message_count,
                "hidden": conversation.hidden,
                "peer_deleted": conversation.peer_deleted,
                "started_by_agent": conversation.started_by_agent,
            }));
        }
        audit.count = Some(listed.len());
        Ok(Json(json!({ "conversations": listed })).into_response())
    }
    .await;
    finish(result, audit)
}

#[derive(Deserialize)]
pub(crate) struct HistoryQuery {
    #[serde(default)]
    before_seq: Option<String>,
    #[serde(default)]
    limit: Option<String>,
}

fn page_limit(limit: Option<&str>) -> Result<u32, AuthError> {
    match limit {
        None => Ok(50),
        Some(limit) => limit
            .parse::<u32>()
            .ok()
            .filter(|limit| (1..=100).contains(limit))
            .ok_or(AuthError::Invalid(Invalid {
                field: "limit",
                reason: "expected a whole number from 1 to 100",
            })),
    }
}

/// `GET /v1/agents/{agent_id}/conversations/{conversation_id}/messages?before_seq=&limit=`:
/// `{"entries": [...], "has_more"}`, oldest first, the page the agent's own history
/// request gets. Read only: no read position or delivery cursor of the agent moves.
pub(crate) async fn messages(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path((agent_id, conversation_id)): Path<(String, String)>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let conversation = path_user(&conversation_id)?;
        audit.conversation = Some(conversation);
        let Ok(Query(query)) = query else {
            return Err(AuthError::Invalid(Invalid {
                field: "query",
                reason: "expected before_seq and limit only once each",
            }));
        };
        let before_seq = query
            .before_seq
            .as_deref()
            .map(|seq| {
                seq.parse::<i64>()
                    .ok()
                    .filter(|seq| *seq >= 1)
                    .ok_or(AuthError::Invalid(Invalid {
                        field: "before_seq",
                        reason: "expected a whole number of at least 1",
                    }))
            })
            .transpose()?;
        let limit = page_limit(query.limit.as_deref())?;
        let page = api
            .agent_owners
            .history(owner, agent, conversation, before_seq, limit)
            .await?;
        let entries = page
            .entries
            .iter()
            .map(entry_json)
            .collect::<Result<Vec<_>, _>>()?;
        audit.count = Some(entries.len());
        Ok(Json(json!({ "entries": entries, "has_more": page.has_more })).into_response())
    }
    .await;
    finish(result, audit)
}

#[derive(Deserialize)]
pub(crate) struct CallsQuery {
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<String>,
}

/// `GET /v1/agents/{agent_id}/calls?before=<cursor>&limit=N`: the agent's call history as
/// `GET /v1/calls` answers the agent (`{calls, next?}`). Read only.
pub(crate) async fn calls(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(agent_id): Path<String>,
    query: Result<Query<CallsQuery>, QueryRejection>,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let Ok(Query(query)) = query else {
            return Err(AuthError::Invalid(Invalid {
                field: "query",
                reason: "expected before and limit only once each",
            }));
        };
        let limit = page_limit(query.limit.as_deref())?;
        let before = query
            .before
            .as_deref()
            .map(|text| {
                CallCursor::decode(text).ok_or(AuthError::Invalid(Invalid {
                    field: "before",
                    reason: "expected a cursor from an earlier answer",
                }))
            })
            .transpose()?;
        let calls = api.agent_owners.calls(owner, agent, before, limit).await?;
        audit.count = Some(calls.len());
        let mut answer = json!({
            "calls": calls.iter().map(CallEntry::to_json).collect::<Vec<_>>(),
        });
        if let (Some(last), true, Value::Object(fields)) =
            (calls.last(), calls.len() == limit as usize, &mut answer)
        {
            fields.insert("next".to_owned(), Value::String(last.cursor.encode()));
        }
        Ok(Json(answer).into_response())
    }
    .await;
    finish(result, audit)
}

/// `name` if it is one of `known`.
fn known(known: &[&'static str], name: &str) -> Option<&'static str> {
    known.iter().copied().find(|known| *known == name)
}

fn outgoing_json(settings: AgentSettings) -> Value {
    let [requests, chats, groups, calls] = settings.outgoing;
    json!({ "requests": requests, "chats": chats, "groups": groups, "calls": calls })
}

/// `GET /v1/agents/{agent_id}/outgoing-policy`: `{"requests", "chats", "groups", "calls"}`,
/// each `allowed`, `ask` or `off` (spec 2026-11-09 proactive agents, §6.1).
pub(crate) async fn get_outgoing_policy(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(agent_id): Path<String>,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let settings = api.agent_owners.settings(owner, agent).await?;
        audit.outgoing = Some(settings.outgoing);
        Ok(Json(outgoing_json(settings)).into_response())
    }
    .await;
    finish(result, audit)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutgoingBody {
    #[serde(default)]
    requests: Option<String>,
    #[serde(default)]
    chats: Option<String>,
    #[serde(default)]
    groups: Option<String>,
    #[serde(default)]
    calls: Option<String>,
}

/// `PUT /v1/agents/{agent_id}/outgoing-policy` with any of the four kinds: all four now.
/// Actions already waiting keep waiting.
pub(crate) async fn put_outgoing_policy(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(agent_id): Path<String>,
    body: Result<JsonBody<OutgoingBody>, AuthError>,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let JsonBody(body) = body?;
        let mut change = [None; 4];
        let given = [body.requests, body.chats, body.groups, body.calls];
        for ((slot, value), kind) in change.iter_mut().zip(given).zip(OUTGOING_KINDS) {
            if let Some(value) = value {
                *slot = Some(known(&OUTGOING_POLICIES, &value).ok_or(AuthError::Invalid(
                    Invalid {
                        field: kind,
                        reason: "expected allowed, ask or off",
                    },
                ))?);
            }
        }
        let settings = api.agent_owners.set_outgoing(owner, agent, change).await?;
        audit.outgoing = Some(settings.outgoing);
        Ok(Json(outgoing_json(settings)).into_response())
    }
    .await;
    finish(result, audit)
}

/// `GET /v1/agents/{agent_id}/inbound-policy`: `{"policy": "owner_only"}`.
pub(crate) async fn get_inbound_policy(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(agent_id): Path<String>,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let settings = api.agent_owners.settings(owner, agent).await?;
        audit.policy = Some(settings.inbound);
        Ok(Json(json!({ "policy": settings.inbound })).into_response())
    }
    .await;
    finish(result, audit)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InboundBody {
    policy: String,
}

/// `PUT /v1/agents/{agent_id}/inbound-policy {"policy"}`: the policy now.
pub(crate) async fn put_inbound_policy(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(agent_id): Path<String>,
    body: Result<JsonBody<InboundBody>, AuthError>,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let JsonBody(body) = body?;
        let policy = known(&INBOUND_POLICIES, &body.policy).ok_or(AuthError::Invalid(Invalid {
            field: "policy",
            reason: "expected owner_only, owner_agents or contacts",
        }))?;
        let settings = api.agent_owners.set_inbound(owner, agent, policy).await?;
        audit.policy = Some(settings.inbound);
        Ok(Json(json!({ "policy": settings.inbound })).into_response())
    }
    .await;
    finish(result, audit)
}

/// The profiles of everyone `actions` name, as `agent` may see them, by user.
async fn action_profiles(
    api: &Api,
    agent: Uuid,
    actions: &[OutgoingAction],
) -> Result<std::collections::HashMap<Uuid, Value>, AuthError> {
    let users: Vec<Uuid> = actions
        .iter()
        .flat_map(|action| action.target.iter().chain(&action.members).copied())
        .collect();
    let profiles = profiles_of(api, agent, &users).await?;
    Ok(users.into_iter().zip(profiles).collect())
}

/// One action as the API answers it.
fn action_json(
    action: &OutgoingAction,
    profiles: &std::collections::HashMap<Uuid, Value>,
) -> Value {
    let profile = |user: &Uuid| profiles.get(user).cloned().unwrap_or(Value::Null);
    json!({
        "action_id": action.action_id,
        "kind": action.kind,
        "target": action.target.as_ref().map_or(Value::Null, profile),
        "conversation_id": action.conversation,
        "title": action.title,
        "members": action.members.iter().map(profile).collect::<Vec<_>>(),
        "state": action.state,
        "error": action.error,
        "created_at": rfc3339(action.created_at),
        "expires_at": rfc3339(action.created_at + ACTION_EXPIRY),
        "resolved_at": action.resolved_at.map(rfc3339),
    })
}

/// `GET /v1/agents/{agent_id}/outgoing`: `{"actions": [<action>]}`, the actions waiting for
/// the owner, oldest first.
pub(crate) async fn list_outgoing(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(agent_id): Path<String>,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let actions = api.agent_owners.outgoing(owner, agent).await?;
        let profiles = action_profiles(&api, agent, &actions).await?;
        audit.count = Some(actions.len());
        let actions: Vec<Value> = actions
            .iter()
            .map(|action| action_json(action, &profiles))
            .collect();
        Ok(Json(json!({ "actions": actions })).into_response())
    }
    .await;
    finish(result, audit)
}

async fn resolve_outgoing(
    api: Api,
    headers: HeaderMap,
    ip: Option<std::net::IpAddr>,
    (agent_id, action_id): (String, String),
    approve: bool,
) -> Response {
    let mut audit = OwnerAudit::default();
    let result = async {
        let (owner, agent) = owner_of(&api, &headers, ip, &agent_id, &mut audit).await?;
        let action_id = path_user(&action_id)?;
        audit.action = Some(action_id);
        let action = api
            .agent_owners
            .resolve_outgoing(owner, agent, action_id, approve)
            .await?;
        audit.kind = Some(action.kind);
        if !approve {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        // The action is carried out already: a failed profile lookup must not turn it into
        // an error (a retry would be `not_found`), so the users are answered by id only.
        let profiles = match action_profiles(&api, agent, std::slice::from_ref(&action)).await {
            Ok(profiles) => profiles,
            Err(_) => action
                .target
                .iter()
                .chain(&action.members)
                .map(|user| (*user, hidden_profile(*user)))
                .collect(),
        };
        Ok(Json(json!({ "action": action_json(&action, &profiles) })).into_response())
    }
    .await;
    finish(result, audit)
}

/// `POST /v1/agents/{agent_id}/outgoing/{action_id}/approve`: `{"action": <action>}`,
/// `approved` or `failed` (with the error); an action that is not waiting is `not_found`.
pub(crate) async fn approve_outgoing(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(ids): Path<(String, String)>,
) -> Response {
    resolve_outgoing(api, headers, ip, ids, true).await
}

/// `POST /v1/agents/{agent_id}/outgoing/{action_id}/decline`: 204; an action that is not
/// waiting is `not_found`.
pub(crate) async fn decline_outgoing(
    State(api): State<Api>,
    headers: HeaderMap,
    ClientIp(ip): ClientIp,
    Path(ids): Path<(String, String)>,
) -> Response {
    resolve_outgoing(api, headers, ip, ids, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_svc_actions_read_as_the_api_names_them() {
        let (action, group, member) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let wire = v1::OutgoingAction {
            action_id: id_bytes(action),
            agent_user_id: id_bytes(Uuid::now_v7()),
            kind: OutgoingKind::Invite.into(),
            target_user_id: Vec::new(),
            conversation_id: id_bytes(group),
            title: String::new(),
            member_ids: vec![id_bytes(member)],
            state: OutgoingState::Failed.into(),
            error: ErrorCode::NotContact.into(),
            created_at: 1_790_000_000_000_000,
            resolved_at: 1_790_000_060_000_000,
        };
        let read = action_of(&wire).unwrap();
        assert_eq!(
            read,
            OutgoingAction {
                action_id: action,
                kind: "invite",
                target: None,
                conversation: Some(group),
                title: None,
                members: vec![member],
                state: "failed",
                error: Some("not_contact".to_owned()),
                created_at: DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
                resolved_at: DateTime::from_timestamp(1_790_000_060, 0),
            }
        );
        let waiting = v1::OutgoingAction {
            state: OutgoingState::OwnerDeclined.into(),
            error: ErrorCode::Unspecified.into(),
            resolved_at: 0,
            title: "Plans".to_owned(),
            ..wire.clone()
        };
        let read = action_of(&waiting).unwrap();
        assert_eq!(
            (
                read.state,
                read.error,
                read.resolved_at,
                read.title.as_deref()
            ),
            ("declined", None, None, Some("Plans"))
        );
        let unknown = v1::OutgoingAction {
            kind: OutgoingKind::Unspecified.into(),
            ..wire
        };
        assert_eq!(action_of(&unknown), Err(AuthError::Unavailable));
    }

    #[test]
    fn an_action_that_is_not_waiting_is_not_found() {
        let status = push_proto::error_status(
            tonic::Code::NotFound,
            v1::Error {
                code: ErrorCode::TargetNotFound.into(),
                message: "no such waiting action".to_owned(),
                ..v1::Error::default()
            },
        );
        assert_eq!(refused("ResolveOutgoing", &status), AuthError::NotFound);
    }
}
