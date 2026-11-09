//! An agent's owner acting on the agent and reading its chats (spec 2026-11-06 agent
//! connections and the owner view, spec 2026-11-09 proactive agents): auth-svc's
//! `/v1/agents/{agent_id}/…` routes, for a signed-in owner. Everyone else gets `not_found`. Nothing here is stored locally.

use super::Client;
use crate::calls::CallPage;
use crate::outgoing::InboundPolicy;
use crate::owners::{
    AgentContact, AgentConversation, AgentHistoryPage, AgentPolicy, AgentRequest, Decision,
    OutgoingPolicies, OutgoingPolicyUpdate, OwnerOutgoingAction,
};
use crate::{ConversationId, SdkError, UserId};

macro_rules! as_owner {
    ($client:expr, |$inner:ident, $token:ident| $call:expr) => {{
        let $inner = $client.inner.clone();
        $client
            .run(async move {
                let session = $inner.credentials.fresh().await?;
                let $token = session.access_token.as_str();
                $call.await
            })
            .await
    }};
}

impl Client {
    /// The agent's connection policy.
    pub async fn agent_policy(&self, agent: UserId) -> Result<AgentPolicy, SdkError> {
        as_owner!(self, |inner, token| inner.auth.agent_policy(token, agent))
    }

    /// Sets the agent's connection policy; requests already waiting keep waiting.
    pub async fn set_agent_policy(
        &self,
        agent: UserId,
        policy: AgentPolicy,
    ) -> Result<AgentPolicy, SdkError> {
        as_owner!(self, |inner, token| inner
            .auth
            .set_agent_policy(token, agent, policy))
    }

    /// The requests waiting for the owner's decision, oldest first.
    pub async fn agent_requests(&self, agent: UserId) -> Result<Vec<AgentRequest>, SdkError> {
        as_owner!(self, |inner, token| inner.auth.agent_requests(token, agent))
    }

    /// Approves a waiting request: the requester and the agent become contacts.
    /// `not_found` without a waiting request.
    pub async fn approve_agent_request(&self, agent: UserId, user: UserId) -> Result<(), SdkError> {
        as_owner!(self, |inner, token| inner.auth.decide(
            token,
            agent,
            user,
            Decision::Approve
        ))
    }

    /// Declines a waiting request: the requester sees an ordinary decline.
    pub async fn decline_agent_request(&self, agent: UserId, user: UserId) -> Result<(), SdkError> {
        as_owner!(self, |inner, token| inner.auth.decide(
            token,
            agent,
            user,
            Decision::Decline
        ))
    }

    /// Blocks `user` for the agent, whether or not a request waits.
    pub async fn block_for_agent(&self, agent: UserId, user: UserId) -> Result<(), SdkError> {
        as_owner!(self, |inner, token| inner.auth.decide(
            token,
            agent,
            user,
            Decision::Block
        ))
    }

    /// The agent's relationships, as the agent sees them.
    pub async fn agent_contacts(&self, agent: UserId) -> Result<Vec<AgentContact>, SdkError> {
        as_owner!(self, |inner, token| inner.auth.agent_contacts(token, agent))
    }

    /// Ends the agent's relationship with `user` (never the owner's: `owner_contact`).
    pub async fn remove_agent_contact(&self, agent: UserId, user: UserId) -> Result<(), SdkError> {
        as_owner!(self, |inner, token| inner
            .auth
            .remove_agent_contact(token, agent, user))
    }

    /// The agent's conversations, newest activity first (read only).
    pub async fn agent_conversations(
        &self,
        agent: UserId,
    ) -> Result<Vec<AgentConversation>, SdkError> {
        as_owner!(self, |inner, token| inner
            .auth
            .agent_conversations(token, agent))
    }

    /// A page of one of the agent's conversations, exactly as the agent sees it (read
    /// only: the agent's read state does not change).
    pub async fn agent_history(
        &self,
        agent: UserId,
        conversation: ConversationId,
        before_seq: Option<i64>,
        limit: Option<u32>,
    ) -> Result<AgentHistoryPage, SdkError> {
        as_owner!(self, |inner, token| inner.auth.agent_history(
            token,
            agent,
            conversation,
            before_seq,
            limit
        ))
    }

    /// A page of the agent's call history, newest first.
    pub async fn agent_calls(
        &self,
        agent: UserId,
        before: Option<&str>,
        limit: Option<u32>,
    ) -> Result<CallPage, SdkError> {
        let before = before.map(str::to_owned);
        as_owner!(self, |inner, token| inner.auth.agent_calls(
            token,
            agent,
            before.as_deref(),
            limit
        ))
    }

    /// What the agent may start on its own: contact requests, new chats, groups and
    /// calls to others, each `allowed`, `ask` or `off`.
    pub async fn outgoing_policy(&self, agent: UserId) -> Result<OutgoingPolicies, SdkError> {
        as_owner!(self, |inner, token| inner
            .auth
            .outgoing_policy(token, agent))
    }

    /// Changes the kinds `update` gives; the policies now. Actions already waiting keep
    /// waiting.
    pub async fn set_outgoing_policy(
        &self,
        agent: UserId,
        update: OutgoingPolicyUpdate,
    ) -> Result<OutgoingPolicies, SdkError> {
        as_owner!(self, |inner, token| inner
            .auth
            .set_outgoing_policy(token, agent, update))
    }

    /// Whose messages reach the agent's backend.
    pub async fn inbound_policy(&self, agent: UserId) -> Result<InboundPolicy, SdkError> {
        as_owner!(self, |inner, token| inner.auth.inbound_policy(token, agent))
    }

    /// Sets whose messages reach the agent's backend; the policy now.
    pub async fn set_inbound_policy(
        &self,
        agent: UserId,
        policy: InboundPolicy,
    ) -> Result<InboundPolicy, SdkError> {
        as_owner!(self, |inner, token| inner
            .auth
            .set_inbound_policy(token, agent, policy))
    }

    /// The agent's actions waiting for the owner's decision, oldest first. (An agent
    /// lists its own actions with [`Client::outgoing_actions`].)
    pub async fn agent_outgoing(
        &self,
        agent: UserId,
    ) -> Result<Vec<OwnerOutgoingAction>, SdkError> {
        as_owner!(self, |inner, token| inner.auth.agent_outgoing(token, agent))
    }

    /// Approves a waiting action: it is carried out as the agent. The action answered is
    /// `approved`, or `failed` with the error when the normal rules refused it (the
    /// approval still counts). `not_found` unless it waits.
    pub async fn approve_outgoing(
        &self,
        agent: UserId,
        action_id: &str,
    ) -> Result<OwnerOutgoingAction, SdkError> {
        let action_id = action_id.to_owned();
        as_owner!(self, |inner, token| inner
            .auth
            .approve_outgoing(token, agent, &action_id))
    }

    /// Declines a waiting action: the agent learns it was declined. `not_found` unless it
    /// waits.
    pub async fn decline_outgoing(&self, agent: UserId, action_id: &str) -> Result<(), SdkError> {
        let action_id = action_id.to_owned();
        as_owner!(self, |inner, token| inner
            .auth
            .decline_outgoing(token, agent, &action_id))
    }
}
