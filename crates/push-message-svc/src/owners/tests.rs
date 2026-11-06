use std::sync::Arc;

use push_domain::{InMemoryStore, RecordingNotifier, SystemClock};
use push_proto::error_from_status;
use push_proto::internal::v1::agent_owners_server::AgentOwners;
use push_proto::v1::{ContactState, ErrorCode};
use tonic::Code;

use super::*;

type Svc = Messaging<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;

struct World {
    messaging: Arc<Svc>,
    api: AgentOwnersApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>,
    owner: UserId,
    agent: UserId,
}

async fn world() -> World {
    let messaging = Arc::new(Messaging::new(
        InMemoryStore::default(),
        SystemClock,
        Arc::new(RecordingNotifier::default()),
    ));
    let (owner, agent) = (UserId::new(), UserId::new());
    messaging.enroll_agent(owner, agent).await.unwrap();
    World {
        api: AgentOwnersApi::new(Arc::clone(&messaging)),
        messaging,
        owner,
        agent,
    }
}

fn of(owner: UserId, agent: UserId) -> AgentRef {
    AgentRef {
        owner_user_id: id_bytes(owner.0),
        agent_user_id: id_bytes(agent.0),
    }
}

fn refused(status: Status) -> (Code, ErrorCode) {
    (status.code(), error_from_status(&status).code())
}

#[tokio::test]
async fn the_owner_sets_the_policy_and_resolves_waiting_requests() {
    let world = world().await;
    let me = of(world.owner, world.agent);
    let policy = world
        .api
        .get_policy(Request::new(me.clone()))
        .await
        .unwrap();
    assert_eq!(policy.get_ref().policy(), internal::AgentPolicy::Ask);
    let person = UserId::new();
    world.messaging.register_user(person).await.unwrap();
    world
        .messaging
        .contact_request(person, world.agent)
        .await
        .unwrap();
    let waiting = world
        .api
        .list_waiting(Request::new(me.clone()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(waiting.requests.len(), 1);
    assert_eq!(waiting.requests[0].user_id, id_bytes(person.0));
    world
        .api
        .resolve(Request::new(ResolveRequest {
            agent: Some(me.clone()),
            user_id: id_bytes(person.0),
            action: ResolveAction::Approve.into(),
        }))
        .await
        .unwrap();
    let contacts = world
        .api
        .list_contacts(Request::new(me.clone()))
        .await
        .unwrap()
        .into_inner();
    assert!(
        contacts
            .contacts
            .iter()
            .any(|contact| contact.user_id == id_bytes(person.0)
                && contact.state() == ContactState::Accepted)
    );

    let set = world
        .api
        .set_policy(Request::new(SetAgentPolicyRequest {
            agent: Some(me.clone()),
            policy: internal::AgentPolicy::RefuseAll.into(),
        }))
        .await
        .unwrap();
    assert_eq!(set.get_ref().policy(), internal::AgentPolicy::RefuseAll);
    let unset = world
        .api
        .set_policy(Request::new(SetAgentPolicyRequest {
            agent: Some(me.clone()),
            policy: internal::AgentPolicy::Unspecified.into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(unset.code(), Code::InvalidArgument);

    world
        .api
        .remove_contact(Request::new(AgentPeerRequest {
            agent: Some(me.clone()),
            user_id: id_bytes(person.0),
        }))
        .await
        .unwrap();
    let owner = world
        .api
        .remove_contact(Request::new(AgentPeerRequest {
            agent: Some(me),
            user_id: id_bytes(world.owner.0),
        }))
        .await
        .unwrap_err();
    assert_eq!(
        refused(owner),
        (Code::FailedPrecondition, ErrorCode::NotAllowed)
    );
}

#[tokio::test]
async fn anyone_but_the_owner_finds_no_agent() {
    let world = world().await;
    let stranger = UserId::new();
    world.messaging.register_user(stranger).await.unwrap();
    for (owner, agent) in [
        (stranger, world.agent),
        (world.agent, world.agent),
        (world.owner, stranger),
        (world.owner, UserId::new()),
    ] {
        let status = world
            .api
            .get_policy(Request::new(of(owner, agent)))
            .await
            .unwrap_err();
        assert_eq!(refused(status), (Code::NotFound, ErrorCode::UserNotFound));
        let status = world
            .api
            .list_waiting(Request::new(of(owner, agent)))
            .await
            .unwrap_err();
        assert_eq!(refused(status), (Code::NotFound, ErrorCode::UserNotFound));
    }
}
