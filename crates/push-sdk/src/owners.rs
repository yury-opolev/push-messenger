//! An agent's owner acting on the agent and reading its chats (spec 2026-11-06 agent
//! connections and the owner view): auth-svc's `/v1/agents/{agent_id}/…` routes. Everyone
//! but the owner gets `not_found` from every one of them.

use reqwest::Method;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::auth::{AuthApi, ProfileBody, millis, once, retried, unreadable};
use crate::calls::CallPage;
use crate::model::ContactState;
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
}

fn policy(answer: &serde_json::Value) -> Result<AgentPolicy, SdkError> {
    answer["policy"]
        .as_str()
        .and_then(AgentPolicy::from_name)
        .ok_or_else(|| unreadable("the agent's policy"))
}

#[cfg(test)]
mod tests;
