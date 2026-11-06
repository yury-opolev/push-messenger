//! The `AgentOwners` API (spec 2026-11-06 agent connections and the owner view): an
//! agent's owner, through auth-svc, reads and sets the agent's policy and acts on its
//! relationships. Also the sweeper that releases refused requests.

use std::sync::Arc;
use std::time::Duration;

use push_domain::{AgentPolicy, Clock, Messaging, Notifier, OwnerAction, Store, UserId};
use push_proto::id_bytes;
use push_proto::internal::v1::{
    self as internal, AgentConversations, AgentHistoryRequest, AgentOwnersDone, AgentPeerRequest,
    AgentPolicyReply, AgentRef, CallHistoryPage, ResolveAction, ResolveRequest,
    SetAgentPolicyRequest, WaitingRequest, WaitingRequests, agent_owners_server,
};
use push_proto::v1::{Contact, ContactList, HistoryPage};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

use crate::convert::{contact_state, domain_error, invalid, micros, parse_id};

/// Serves `AgentOwners` over the messaging service.
pub struct AgentOwnersApi<S, C, N> {
    messaging: Arc<Messaging<S, C, N>>,
}

impl<S, C, N> AgentOwnersApi<S, C, N> {
    pub fn new(messaging: Arc<Messaging<S, C, N>>) -> Self {
        Self { messaging }
    }
}

/// The owner and the agent of a request.
fn agent_ref(agent: Option<&AgentRef>) -> Result<(UserId, UserId), Status> {
    let agent = agent.ok_or_else(|| invalid("agent is required"))?;
    Ok((
        parse_id(&agent.owner_user_id, "owner_user_id", UserId)?,
        parse_id(&agent.agent_user_id, "agent_user_id", UserId)?,
    ))
}

pub(crate) fn policy_to_wire(policy: AgentPolicy) -> internal::AgentPolicy {
    match policy {
        AgentPolicy::Ask => internal::AgentPolicy::Ask,
        AgentPolicy::AutoOwn => internal::AgentPolicy::AutoOwn,
        AgentPolicy::AutoOwnerContacts => internal::AgentPolicy::AutoOwnerContacts,
        AgentPolicy::AutoAny => internal::AgentPolicy::AutoAny,
        AgentPolicy::RefuseAll => internal::AgentPolicy::RefuseAll,
    }
}

fn policy_from_wire(policy: internal::AgentPolicy) -> Result<AgentPolicy, Status> {
    Ok(match policy {
        internal::AgentPolicy::Ask => AgentPolicy::Ask,
        internal::AgentPolicy::AutoOwn => AgentPolicy::AutoOwn,
        internal::AgentPolicy::AutoOwnerContacts => AgentPolicy::AutoOwnerContacts,
        internal::AgentPolicy::AutoAny => AgentPolicy::AutoAny,
        internal::AgentPolicy::RefuseAll => AgentPolicy::RefuseAll,
        internal::AgentPolicy::Unspecified => return Err(invalid("policy is required")),
    })
}

fn reply(policy: AgentPolicy) -> Response<AgentPolicyReply> {
    Response::new(AgentPolicyReply {
        policy: policy_to_wire(policy).into(),
    })
}

#[tonic::async_trait]
impl<S, C, N> agent_owners_server::AgentOwners for AgentOwnersApi<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    async fn get_policy(
        &self,
        request: Request<AgentRef>,
    ) -> Result<Response<AgentPolicyReply>, Status> {
        let (owner, agent) = agent_ref(Some(request.get_ref()))?;
        let policy = self
            .messaging
            .agent_policy(owner, agent)
            .await
            .map_err(domain_error)?;
        Ok(reply(policy))
    }

    async fn set_policy(
        &self,
        request: Request<SetAgentPolicyRequest>,
    ) -> Result<Response<AgentPolicyReply>, Status> {
        let request = request.into_inner();
        let (owner, agent) = agent_ref(request.agent.as_ref())?;
        let policy = policy_from_wire(request.policy())?;
        let policy = self
            .messaging
            .set_agent_policy(owner, agent, policy)
            .await
            .map_err(domain_error)?;
        Ok(reply(policy))
    }

    async fn list_waiting(
        &self,
        request: Request<AgentRef>,
    ) -> Result<Response<WaitingRequests>, Status> {
        let (owner, agent) = agent_ref(Some(request.get_ref()))?;
        let waiting = self
            .messaging
            .waiting_requests(owner, agent)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(WaitingRequests {
            requests: waiting
                .into_iter()
                .map(|(user, at)| WaitingRequest {
                    user_id: id_bytes(user.0),
                    requested_at: micros(at),
                })
                .collect(),
        }))
    }

    async fn resolve(
        &self,
        request: Request<ResolveRequest>,
    ) -> Result<Response<AgentOwnersDone>, Status> {
        let request = request.into_inner();
        let (owner, agent) = agent_ref(request.agent.as_ref())?;
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let action = match request.action() {
            ResolveAction::Approve => OwnerAction::Approve,
            ResolveAction::Decline => OwnerAction::Decline,
            ResolveAction::Block => OwnerAction::Block,
            ResolveAction::Unspecified => return Err(invalid("action is required")),
        };
        self.messaging
            .resolve_request(owner, agent, user, action)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(AgentOwnersDone {}))
    }

    async fn list_contacts(
        &self,
        request: Request<AgentRef>,
    ) -> Result<Response<ContactList>, Status> {
        let (owner, agent) = agent_ref(Some(request.get_ref()))?;
        let contacts = self
            .messaging
            .agent_contacts(owner, agent)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(ContactList {
            contacts: contacts
                .into_iter()
                .map(|contact| Contact {
                    user_id: id_bytes(contact.user_id.0),
                    state: contact_state(Some(contact.state)).into(),
                })
                .collect(),
        }))
    }

    async fn remove_contact(
        &self,
        request: Request<AgentPeerRequest>,
    ) -> Result<Response<AgentOwnersDone>, Status> {
        let request = request.into_inner();
        let (owner, agent) = agent_ref(request.agent.as_ref())?;
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        self.messaging
            .remove_agent_contact(owner, agent, user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(AgentOwnersDone {}))
    }

    async fn conversations(
        &self,
        _request: Request<AgentRef>,
    ) -> Result<Response<AgentConversations>, Status> {
        Err(Status::unimplemented("the owner view is not served yet"))
    }

    async fn history(
        &self,
        _request: Request<AgentHistoryRequest>,
    ) -> Result<Response<HistoryPage>, Status> {
        Err(Status::unimplemented("the owner view is not served yet"))
    }

    async fn call_history(
        &self,
        _request: Request<internal::AgentCallHistoryRequest>,
    ) -> Result<Response<CallHistoryPage>, Status> {
        Err(Status::unimplemented("the owner view is not served yet"))
    }
}

/// Releases due refused requests every `every` until `shutdown`. Every replica may run
/// it: a release is idempotent.
pub async fn run_refusal_sweeper<S, C, N>(
    messaging: Arc<Messaging<S, C, N>>,
    every: Duration,
    shutdown: CancellationToken,
) where
    S: Store,
    C: Clock,
    N: Notifier,
{
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(every) => {}
        }
        match messaging.release_refusals().await {
            Ok(0) => {}
            Ok(released) => tracing::info!(released, "refused agent requests released"),
            Err(error) => tracing::warn!(%error, "releasing refused agent requests failed"),
        }
    }
}

#[cfg(test)]
mod tests;
