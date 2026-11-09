//! An agent's owner acting on the agent and reading its chats (spec 2026-11-06 agent
//! connections and the owner view, spec 2026-11-09 proactive agents): auth-svc's
//! `/v1/agents/{agent_id}/…` routes. Everyone but the owner gets `not_found` from every
//! one of them.

use reqwest::Method;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::auth::{AuthApi, ProfileBody, millis, once, retried, unreadable};
use crate::calls::CallPage;
use crate::model::{ContactState, redacted};
use crate::outgoing::{InboundPolicy, OutgoingKind, OutgoingPolicy, OutgoingState};
use crate::{ClientMsgId, ConversationId, MessageId, Profile, SdkError, UserId};

/// Who an agent accepts contact requests from. Set by its owner; `Ask` by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentPolicy {
    /// Every request waits for the owner.
    Ask,
    /// The owner's own agents are accepted; the rest wait.
    AutoOwn,
    /// The owner's accepted contacts, and agents owned by the owner or by those contacts,
    /// are accepted; the rest wait.
    AutoOwnerContacts,
    /// Everyone is accepted.
    AutoAny,
    /// Everyone but the owner is refused (the requester sees an ordinary decline).
    RefuseAll,
}

impl AgentPolicy {
    /// The API's name: `ask`, `auto_own`, `auto_owner_contacts`, `auto_any`, `refuse_all`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AutoOwn => "auto_own",
            Self::AutoOwnerContacts => "auto_owner_contacts",
            Self::AutoAny => "auto_any",
            Self::RefuseAll => "refuse_all",
        }
    }

    /// The policy with this API name.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "ask" => Self::Ask,
            "auto_own" => Self::AutoOwn,
            "auto_owner_contacts" => Self::AutoOwnerContacts,
            "auto_any" => Self::AutoAny,
            "refuse_all" => Self::RefuseAll,
            _ => return None,
        })
    }
}

/// A request waiting for the owner's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    pub user: Profile,
    /// Milliseconds since the Unix epoch.
    pub requested_at_ms: i64,
}

/// One of an agent's relationships, as the agent sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentContact {
    pub user: Profile,
    pub state: ContactState,
    /// The owner's own contact with the agent: it cannot be removed.
    pub owner: bool,
}

/// One of an agent's conversations in its owner's view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConversation {
    pub conversation_id: ConversationId,
    /// `direct` or `group`.
    pub kind: String,
    pub title: Option<String>,
    /// Direct only: the other member.
    pub peer: Option<Profile>,
    /// Direct only, when the peer is an agent: its owner.
    pub peer_owner: Option<Profile>,
    pub member_count: u32,
    /// Milliseconds since the Unix epoch.
    pub last_activity_at_ms: i64,
    /// Messages the agent can see that are not deleted.
    pub message_count: i64,
    pub hidden: bool,
    pub peer_deleted: bool,
    /// A direct chat the agent opened with someone other than the owner, or a group it
    /// created.
    pub started_by_agent: bool,
}

/// One entry of an agent's conversation, as the agent's own client gets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHistoryEntry {
    pub seq: i64,
    pub message_id: MessageId,
    pub client_msg_id: ClientMsgId,
    /// `message` or `system`.
    pub kind: String,
    pub sender_id: UserId,
    /// `text`, `rich`, `answer` or `system`.
    pub content_type: String,
    /// `None` once deleted.
    pub body: Option<String>,
    pub deleted: bool,
    pub answer_seq: Option<i64>,
    pub server_ts_ms: i64,
    pub edited_at_ms: Option<i64>,
}

/// A page of an agent's conversation, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHistoryPage {
    pub entries: Vec<AgentHistoryEntry>,
    /// Older entries exist: ask again with the first entry's `seq` as `before_seq`.
    pub has_more: bool,
}

/// An agent's outgoing policy, one per kind of action (spec 2026-11-09 proactive agents).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutgoingPolicies {
    /// Contact requests.
    pub requests: OutgoingPolicy,
    /// New direct chats with someone other than the owner.
    pub chats: OutgoingPolicy,
    /// Creating groups and adding members to them.
    pub groups: OutgoingPolicy,
    /// Calls to someone other than the owner.
    pub calls: OutgoingPolicy,
}

/// A change of an agent's outgoing policy: the kinds given change, the others stay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct OutgoingPolicyUpdate {
    pub requests: Option<OutgoingPolicy>,
    pub chats: Option<OutgoingPolicy>,
    pub groups: Option<OutgoingPolicy>,
    pub calls: Option<OutgoingPolicy>,
}

/// One of an agent's actions in its owner's view. `Debug` shows a group's title as its
/// length only.
#[derive(Clone, PartialEq, Eq)]
pub struct OwnerOutgoingAction {
    /// The action's id (a UUID).
    pub action_id: String,
    pub kind: OutgoingKind,
    /// Request, chat and call: the other user, as the agent may see them.
    pub target: Option<Profile>,
    /// Invite: the group; once approved, the chat or group it opened or created.
    pub conversation: Option<ConversationId>,
    /// Group: its title.
    pub title: Option<String>,
    /// Group and invite: the members, as the agent may see them.
    pub members: Vec<Profile>,
    pub state: OutgoingState,
    /// Failed: why, as an error code (`not_contact`, `blocked`, …).
    pub error: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub created_at_ms: i64,
    /// When it expires unless the owner decides it first.
    pub expires_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
}

impl std::fmt::Debug for OwnerOutgoingAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OwnerOutgoingAction")
            .field("action_id", &self.action_id)
            .field("kind", &self.kind)
            .field("target", &self.target)
            .field("conversation", &self.conversation)
            .field("title", &redacted(&self.title))
            .field("members", &self.members)
            .field("state", &self.state)
            .field("error", &self.error)
            .field("created_at_ms", &self.created_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("resolved_at_ms", &self.resolved_at_ms)
            .finish()
    }
}

/// An action as auth-svc answers it.
#[derive(Deserialize)]
struct ActionBody {
    action_id: Uuid,
    kind: String,
    target: Option<ProfileBody>,
    conversation_id: Option<Uuid>,
    title: Option<String>,
    #[serde(default)]
    members: Vec<ProfileBody>,
    state: String,
    error: Option<String>,
    created_at: String,
    expires_at: String,
    resolved_at: Option<String>,
}

impl ActionBody {
    fn read(self) -> Result<OwnerOutgoingAction, SdkError> {
        let broken = || unreadable("the agent's actions");
        Ok(OwnerOutgoingAction {
            action_id: self.action_id.to_string(),
            kind: OutgoingKind::from_name(&self.kind).ok_or_else(broken)?,
            target: self.target.map(Profile::from),
            conversation: self.conversation_id.map(ConversationId),
            title: self.title,
            members: self.members.into_iter().map(Profile::from).collect(),
            state: OutgoingState::from_name(&self.state).ok_or_else(broken)?,
            error: self.error,
            created_at_ms: millis(&self.created_at).ok_or_else(broken)?,
            expires_at_ms: millis(&self.expires_at).ok_or_else(broken)?,
            resolved_at_ms: match self.resolved_at {
                Some(time) => Some(millis(&time).ok_or_else(broken)?),
                None => None,
            },
        })
    }
}

/// What the owner does with a waiting request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Approve,
    Decline,
    Block,
}

fn contact_state(name: &str) -> Option<ContactState> {
    Some(match name {
        "requested_out" => ContactState::RequestedOut,
        "requested_in" => ContactState::RequestedIn,
        "accepted" => ContactState::Accepted,
        "blocked" => ContactState::Blocked,
        "none" => ContactState::None,
        _ => return None,
    })
}

impl AuthApi {
    fn agent_path(agent: UserId, rest: &str) -> String {
        format!("/v1/agents/{agent}{rest}")
    }

    async fn agent_get(
        &self,
        access_token: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<serde_json::Value, SdkError> {
        retried(crate::auth::ORDINARY, || {
            self.request(Method::GET, path)
                .query(query)
                .bearer_auth(access_token)
        })
        .await
    }

    /// `GET /v1/agents/{agent}/policy`.
    pub async fn agent_policy(
        &self,
        access_token: &str,
        agent: UserId,
    ) -> Result<AgentPolicy, SdkError> {
        let answer = self
            .agent_get(access_token, &Self::agent_path(agent, "/policy"), &[])
            .await?;
        policy(&answer)
    }

    /// `PUT /v1/agents/{agent}/policy`.
    pub async fn set_agent_policy(
        &self,
        access_token: &str,
        agent: UserId,
        policy_now: AgentPolicy,
    ) -> Result<AgentPolicy, SdkError> {
        let path = Self::agent_path(agent, "/policy");
        let answer = retried(crate::auth::ORDINARY, || {
            self.request(Method::PUT, &path)
                .bearer_auth(access_token)
                .json(&json!({ "policy": policy_now.as_str() }))
        })
        .await?;
        policy(&answer)
    }

    /// `GET /v1/agents/{agent}/requests`.
    pub async fn agent_requests(
        &self,
        access_token: &str,
        agent: UserId,
    ) -> Result<Vec<AgentRequest>, SdkError> {
        #[derive(Deserialize)]
        struct RequestBody {
            user: ProfileBody,
            requested_at: String,
        }
        #[derive(Deserialize)]
        struct Body {
            requests: Vec<RequestBody>,
        }
        let answer = self
            .agent_get(access_token, &Self::agent_path(agent, "/requests"), &[])
            .await?;
        let body: Body =
            serde_json::from_value(answer).map_err(|_| unreadable("the waiting requests"))?;
        body.requests
            .into_iter()
            .map(|request| {
                Ok(AgentRequest {
                    user: request.user.into(),
                    requested_at_ms: millis(&request.requested_at)
                        .ok_or_else(|| unreadable("the waiting requests"))?,
                })
            })
            .collect()
    }

    /// `POST /v1/agents/{agent}/requests/{user}/approve|decline|block`.
    pub(crate) async fn decide(
        &self,
        access_token: &str,
        agent: UserId,
        user: UserId,
        decision: Decision,
    ) -> Result<(), SdkError> {
        let action = match decision {
            Decision::Approve => "approve",
            Decision::Decline => "decline",
            Decision::Block => "block",
        };
        let path = Self::agent_path(agent, &format!("/requests/{user}/{action}"));
        once(self.request(Method::POST, &path).bearer_auth(access_token)).await?;
        Ok(())
    }

    /// `GET /v1/agents/{agent}/contacts`.
    pub async fn agent_contacts(
        &self,
        access_token: &str,
        agent: UserId,
    ) -> Result<Vec<AgentContact>, SdkError> {
        #[derive(Deserialize)]
        struct ContactBody {
            user: ProfileBody,
            state: String,
            owner: bool,
        }
        #[derive(Deserialize)]
        struct Body {
            contacts: Vec<ContactBody>,
        }
        let answer = self
            .agent_get(access_token, &Self::agent_path(agent, "/contacts"), &[])
            .await?;
        let body: Body =
            serde_json::from_value(answer).map_err(|_| unreadable("the agent's contacts"))?;
        body.contacts
            .into_iter()
            .map(|contact| {
                Ok(AgentContact {
                    user: contact.user.into(),
                    state: contact_state(&contact.state)
                        .ok_or_else(|| unreadable("the agent's contacts"))?,
                    owner: contact.owner,
                })
            })
            .collect()
    }

    /// `DELETE /v1/agents/{agent}/contacts/{user}`.
    pub async fn remove_agent_contact(
        &self,
        access_token: &str,
        agent: UserId,
        user: UserId,
    ) -> Result<(), SdkError> {
        let path = Self::agent_path(agent, &format!("/contacts/{user}"));
        retried(crate::auth::ORDINARY, || {
            self.request(Method::DELETE, &path)
                .bearer_auth(access_token)
        })
        .await?;
        Ok(())
    }

    /// `GET /v1/agents/{agent}/conversations`.
    pub async fn agent_conversations(
        &self,
        access_token: &str,
        agent: UserId,
    ) -> Result<Vec<AgentConversation>, SdkError> {
        #[derive(Deserialize)]
        struct ConversationBody {
            conversation_id: Uuid,
            kind: String,
            title: Option<String>,
            peer: Option<ProfileBody>,
            peer_owner: Option<ProfileBody>,
            member_count: u32,
            last_activity_at: String,
            message_count: i64,
            #[serde(default)]
            hidden: bool,
            #[serde(default)]
            peer_deleted: bool,
            #[serde(default)]
            started_by_agent: bool,
        }
        #[derive(Deserialize)]
        struct Body {
            conversations: Vec<ConversationBody>,
        }
        let answer = self
            .agent_get(
                access_token,
                &Self::agent_path(agent, "/conversations"),
                &[],
            )
            .await?;
        let body: Body =
            serde_json::from_value(answer).map_err(|_| unreadable("the agent's conversations"))?;
        body.conversations
            .into_iter()
            .map(|conversation| {
                Ok(AgentConversation {
                    conversation_id: ConversationId(conversation.conversation_id),
                    kind: conversation.kind,
                    title: conversation.title,
                    peer: conversation.peer.map(Profile::from),
                    peer_owner: conversation.peer_owner.map(Profile::from),
                    member_count: conversation.member_count,
                    last_activity_at_ms: millis(&conversation.last_activity_at)
                        .ok_or_else(|| unreadable("the agent's conversations"))?,
                    message_count: conversation.message_count,
                    hidden: conversation.hidden,
                    peer_deleted: conversation.peer_deleted,
                    started_by_agent: conversation.started_by_agent,
                })
            })
            .collect()
    }

    /// `GET /v1/agents/{agent}/conversations/{conversation}/messages`.
    pub async fn agent_history(
        &self,
        access_token: &str,
        agent: UserId,
        conversation: ConversationId,
        before_seq: Option<i64>,
        limit: Option<u32>,
    ) -> Result<AgentHistoryPage, SdkError> {
        #[derive(Deserialize)]
        struct EntryBody {
            seq: i64,
            message_id: Uuid,
            client_msg_id: Uuid,
            kind: String,
            sender_id: Uuid,
            content_type: String,
            body: Option<String>,
            deleted: bool,
            answer_seq: Option<i64>,
            server_ts: String,
            edited_at: Option<String>,
        }
        #[derive(Deserialize)]
        struct Body {
            entries: Vec<EntryBody>,
            has_more: bool,
        }
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(before_seq) = before_seq {
            query.push(("before_seq", before_seq.to_string()));
        }
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        let path = Self::agent_path(agent, &format!("/conversations/{conversation}/messages"));
        let answer = self.agent_get(access_token, &path, &query).await?;
        let body: Body =
            serde_json::from_value(answer).map_err(|_| unreadable("the agent's history"))?;
        let entries = body
            .entries
            .into_iter()
            .map(|entry| {
                Some(AgentHistoryEntry {
                    seq: entry.seq,
                    message_id: MessageId(entry.message_id),
                    client_msg_id: ClientMsgId(entry.client_msg_id),
                    kind: entry.kind,
                    sender_id: UserId(entry.sender_id),
                    content_type: entry.content_type,
                    body: entry.body,
                    deleted: entry.deleted,
                    answer_seq: entry.answer_seq,
                    server_ts_ms: millis(&entry.server_ts)?,
                    edited_at_ms: match entry.edited_at {
                        Some(time) => Some(millis(&time)?),
                        None => None,
                    },
                })
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| unreadable("the agent's history"))?;
        Ok(AgentHistoryPage {
            entries,
            has_more: body.has_more,
        })
    }

    /// `GET /v1/agents/{agent}/calls`.
    pub async fn agent_calls(
        &self,
        access_token: &str,
        agent: UserId,
        before: Option<&str>,
        limit: Option<u32>,
    ) -> Result<CallPage, SdkError> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(before) = before {
            query.push(("before", before.to_owned()));
        }
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        let answer = self
            .agent_get(access_token, &Self::agent_path(agent, "/calls"), &query)
            .await?;
        crate::auth::call_page(answer)
    }

    /// `GET /v1/agents/{agent}/outgoing-policy`.
    pub async fn outgoing_policy(
        &self,
        access_token: &str,
        agent: UserId,
    ) -> Result<OutgoingPolicies, SdkError> {
        let answer = self
            .agent_get(
                access_token,
                &Self::agent_path(agent, "/outgoing-policy"),
                &[],
            )
            .await?;
        outgoing_policies(&answer)
    }

    /// `PUT /v1/agents/{agent}/outgoing-policy` with the kinds `update` gives.
    pub async fn set_outgoing_policy(
        &self,
        access_token: &str,
        agent: UserId,
        update: OutgoingPolicyUpdate,
    ) -> Result<OutgoingPolicies, SdkError> {
        let mut body = serde_json::Map::new();
        for (kind, policy) in [
            ("requests", update.requests),
            ("chats", update.chats),
            ("groups", update.groups),
            ("calls", update.calls),
        ] {
            if let Some(policy) = policy {
                body.insert(kind.to_owned(), json!(policy.as_str()));
            }
        }
        let path = Self::agent_path(agent, "/outgoing-policy");
        let answer = retried(crate::auth::ORDINARY, || {
            self.request(Method::PUT, &path)
                .bearer_auth(access_token)
                .json(&body)
        })
        .await?;
        outgoing_policies(&answer)
    }

    /// `GET /v1/agents/{agent}/inbound-policy`.
    pub async fn inbound_policy(
        &self,
        access_token: &str,
        agent: UserId,
    ) -> Result<InboundPolicy, SdkError> {
        let answer = self
            .agent_get(
                access_token,
                &Self::agent_path(agent, "/inbound-policy"),
                &[],
            )
            .await?;
        inbound_policy(&answer)
    }

    /// `PUT /v1/agents/{agent}/inbound-policy`.
    pub async fn set_inbound_policy(
        &self,
        access_token: &str,
        agent: UserId,
        policy_now: InboundPolicy,
    ) -> Result<InboundPolicy, SdkError> {
        let path = Self::agent_path(agent, "/inbound-policy");
        let answer = retried(crate::auth::ORDINARY, || {
            self.request(Method::PUT, &path)
                .bearer_auth(access_token)
                .json(&json!({ "policy": policy_now.as_str() }))
        })
        .await?;
        inbound_policy(&answer)
    }

    /// `GET /v1/agents/{agent}/outgoing`: the actions waiting for the owner.
    pub async fn agent_outgoing(
        &self,
        access_token: &str,
        agent: UserId,
    ) -> Result<Vec<OwnerOutgoingAction>, SdkError> {
        #[derive(Deserialize)]
        struct Body {
            actions: Vec<ActionBody>,
        }
        let answer = self
            .agent_get(access_token, &Self::agent_path(agent, "/outgoing"), &[])
            .await?;
        let body: Body =
            serde_json::from_value(answer).map_err(|_| unreadable("the agent's actions"))?;
        body.actions.into_iter().map(ActionBody::read).collect()
    }

    /// `POST /v1/agents/{agent}/outgoing/{action_id}/approve`: the action, approved or
    /// failed. Sent once: a repeat is `not_found`.
    pub async fn approve_outgoing(
        &self,
        access_token: &str,
        agent: UserId,
        action_id: &str,
    ) -> Result<OwnerOutgoingAction, SdkError> {
        #[derive(Deserialize)]
        struct Body {
            action: ActionBody,
        }
        let path = Self::outgoing_path(agent, action_id, "approve")?;
        let answer = once(self.request(Method::POST, &path).bearer_auth(access_token)).await?;
        let body: Body =
            serde_json::from_value(answer).map_err(|_| unreadable("the agent's actions"))?;
        body.action.read()
    }

    /// `POST /v1/agents/{agent}/outgoing/{action_id}/decline`. Sent once: a repeat is
    /// `not_found`.
    pub async fn decline_outgoing(
        &self,
        access_token: &str,
        agent: UserId,
        action_id: &str,
    ) -> Result<(), SdkError> {
        let path = Self::outgoing_path(agent, action_id, "decline")?;
        once(self.request(Method::POST, &path).bearer_auth(access_token)).await?;
        Ok(())
    }

    /// The path of a decision on an action; `invalid` unless `action_id` is a UUID.
    fn outgoing_path(agent: UserId, action_id: &str, decision: &str) -> Result<String, SdkError> {
        let action: Uuid = action_id
            .parse()
            .map_err(|_| SdkError::Invalid("the action id is not an id".to_owned()))?;
        Ok(Self::agent_path(
            agent,
            &format!("/outgoing/{action}/{decision}"),
        ))
    }
}

fn outgoing_policies(answer: &serde_json::Value) -> Result<OutgoingPolicies, SdkError> {
    let read = |kind: &str| {
        answer[kind]
            .as_str()
            .and_then(OutgoingPolicy::from_name)
            .ok_or_else(|| unreadable("the agent's outgoing policy"))
    };
    Ok(OutgoingPolicies {
        requests: read("requests")?,
        chats: read("chats")?,
        groups: read("groups")?,
        calls: read("calls")?,
    })
}

fn inbound_policy(answer: &serde_json::Value) -> Result<InboundPolicy, SdkError> {
    answer["policy"]
        .as_str()
        .and_then(InboundPolicy::from_name)
        .ok_or_else(|| unreadable("the agent's inbound policy"))
}

fn policy(answer: &serde_json::Value) -> Result<AgentPolicy, SdkError> {
    answer["policy"]
        .as_str()
        .and_then(AgentPolicy::from_name)
        .ok_or_else(|| unreadable("the agent's policy"))
}

#[cfg(test)]
mod tests;
