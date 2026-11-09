use std::sync::Arc;

use push_domain::{DomainError, InMemoryStore, RecordingNotifier, SystemClock};
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

#[tokio::test]
async fn the_owner_view_answers_what_the_agent_sees() {
    let world = world().await;
    let me = of(world.owner, world.agent);
    world
        .messaging
        .set_agent_policy(world.owner, world.agent, push_domain::AgentPolicy::AutoAny)
        .await
        .unwrap();
    let person = UserId::new();
    world.messaging.register_user(person).await.unwrap();
    let device = push_domain::DeviceId::new();
    world
        .messaging
        .register_device(device, person)
        .await
        .unwrap();
    world
        .messaging
        .contact_request(person, world.agent)
        .await
        .unwrap();
    let chat = world
        .messaging
        .open_direct(person, world.agent)
        .await
        .unwrap();
    world
        .messaging
        .send_text(push_domain::SendText {
            sender: person,
            device,
            conversation: chat,
            client_msg_id: push_domain::ClientMsgId::new(),
            body: "hi agent".into(),
            client_ts: None,
        })
        .await
        .unwrap();
    let listed = world
        .api
        .conversations(Request::new(me.clone()))
        .await
        .unwrap()
        .into_inner();
    let found = listed
        .conversations
        .iter()
        .find(|conversation| conversation.conversation_id == id_bytes(chat.0))
        .unwrap();
    assert_eq!(found.peer_user_id, Some(id_bytes(person.0)));
    assert_eq!(found.message_count, 1);
    let page = world
        .api
        .history(Request::new(AgentHistoryRequest {
            agent: Some(me.clone()),
            conversation_id: id_bytes(chat.0),
            before_seq: None,
            limit: 0,
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(
        page.entries[0].body.as_deref(),
        Some(b"hi agent".as_slice())
    );
    let calls = world
        .api
        .call_history(Request::new(internal::AgentCallHistoryRequest {
            agent: Some(me),
            before: None,
            limit: 10,
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(calls.calls.is_empty());
    let stranger = world
        .api
        .conversations(Request::new(of(person, world.agent)))
        .await
        .unwrap_err();
    assert_eq!(refused(stranger), (Code::NotFound, ErrorCode::UserNotFound));
}

#[tokio::test]
async fn the_owner_sets_the_agents_policies_and_decides_its_actions() {
    let world = world().await;
    let me = of(world.owner, world.agent);
    let settings = |reply: Response<AgentSettingsReply>| reply.into_inner().settings.unwrap();
    let read = settings(
        world
            .api
            .get_settings(Request::new(me.clone()))
            .await
            .unwrap(),
    );
    assert_eq!(read, agent_settings(&AgentSettings::default()));
    let changed = settings(
        world
            .api
            .set_outgoing(Request::new(SetOutgoingRequest {
                agent: Some(me.clone()),
                chats: Some(v1::OutgoingPolicy::Allowed.into()),
                calls: Some(v1::OutgoingPolicy::Ask.into()),
                ..SetOutgoingRequest::default()
            }))
            .await
            .unwrap(),
    );
    assert_eq!(
        (
            changed.requests(),
            changed.chats(),
            changed.groups(),
            changed.calls()
        ),
        (
            v1::OutgoingPolicy::Ask,
            v1::OutgoingPolicy::Allowed,
            v1::OutgoingPolicy::Off,
            v1::OutgoingPolicy::Ask
        )
    );
    let bad = world
        .api
        .set_outgoing(Request::new(SetOutgoingRequest {
            agent: Some(me.clone()),
            groups: Some(v1::OutgoingPolicy::Unspecified.into()),
            ..SetOutgoingRequest::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(refused(bad), (Code::InvalidArgument, ErrorCode::Invalid));
    let inbound = settings(
        world
            .api
            .set_inbound(Request::new(SetInboundRequest {
                agent: Some(me.clone()),
                policy: v1::InboundPolicy::Contacts.into(),
            }))
            .await
            .unwrap(),
    );
    assert_eq!(inbound.inbound(), v1::InboundPolicy::Contacts);
    assert_eq!(inbound.chats(), v1::OutgoingPolicy::Allowed);
    let bad = world
        .api
        .set_inbound(Request::new(SetInboundRequest {
            agent: Some(me.clone()),
            policy: 9,
        }))
        .await
        .unwrap_err();
    assert_eq!(bad.code(), Code::InvalidArgument);

    // Two contact requests wait (the default); one is approved, one declined.
    let (first, second) = (UserId::new(), UserId::new());
    for person in [first, second] {
        world.messaging.register_user(person).await.unwrap();
        let answer = world.messaging.contact_request(world.agent, person).await;
        assert!(matches!(answer, Err(DomainError::WaitingForOwner { .. })));
    }
    let listed = world
        .api
        .list_outgoing(Request::new(me.clone()))
        .await
        .unwrap()
        .into_inner()
        .actions;
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|action| {
        action.state() == v1::OutgoingState::Waiting
            && action.kind() == v1::OutgoingKind::Request
            && action.agent_user_id == id_bytes(world.agent.0)
    }));
    assert_eq!(listed[0].target_user_id, id_bytes(first.0));
    let resolve = |action_id: Vec<u8>, approve| ResolveOutgoingRequest {
        agent: Some(me.clone()),
        action_id,
        approve,
    };
    let approved = world
        .api
        .resolve_outgoing(Request::new(resolve(listed[0].action_id.clone(), true)))
        .await
        .unwrap()
        .into_inner()
        .action
        .unwrap();
    assert_eq!(approved.state(), v1::OutgoingState::Approved);
    assert_eq!(
        world.messaging.contact_state(world.agent, first).await,
        Ok(Some(push_domain::ContactState::RequestedOut))
    );
    let declined = world
        .api
        .resolve_outgoing(Request::new(resolve(listed[1].action_id.clone(), false)))
        .await
        .unwrap()
        .into_inner()
        .action
        .unwrap();
    assert_eq!(declined.state(), v1::OutgoingState::OwnerDeclined);
    // Resolved: no longer waiting.
    let again = world
        .api
        .resolve_outgoing(Request::new(resolve(listed[1].action_id.clone(), true)))
        .await
        .unwrap_err();
    assert_eq!(refused(again), (Code::NotFound, ErrorCode::TargetNotFound));
    assert!(
        world
            .api
            .list_outgoing(Request::new(me.clone()))
            .await
            .unwrap()
            .into_inner()
            .actions
            .is_empty()
    );
    let malformed = world
        .api
        .resolve_outgoing(Request::new(resolve(vec![1, 2, 3], true)))
        .await
        .unwrap_err();
    assert_eq!(malformed.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn anyone_but_the_owner_finds_no_agent_settings_or_actions() {
    let world = world().await;
    world.messaging.register_user(UserId::new()).await.unwrap();
    let person = UserId::new();
    world.messaging.register_user(person).await.unwrap();
    let waiting = world.messaging.contact_request(world.agent, person).await;
    let Err(DomainError::WaitingForOwner { action_id }) = waiting else {
        panic!("not waiting: {waiting:?}");
    };
    let stranger = UserId::new();
    world.messaging.register_user(stranger).await.unwrap();
    for (owner, agent) in [
        (stranger, world.agent),
        (world.agent, world.agent),
        (world.owner, stranger),
        (world.owner, UserId::new()),
    ] {
        let not_found = (Code::NotFound, ErrorCode::UserNotFound);
        let status = world
            .api
            .get_settings(Request::new(of(owner, agent)))
            .await
            .unwrap_err();
        assert_eq!(refused(status), not_found);
        let status = world
            .api
            .set_outgoing(Request::new(SetOutgoingRequest {
                agent: Some(of(owner, agent)),
                calls: Some(v1::OutgoingPolicy::Allowed.into()),
                ..SetOutgoingRequest::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(refused(status), not_found);
        let status = world
            .api
            .set_inbound(Request::new(SetInboundRequest {
                agent: Some(of(owner, agent)),
                policy: v1::InboundPolicy::Contacts.into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(refused(status), not_found);
        let status = world
            .api
            .list_outgoing(Request::new(of(owner, agent)))
            .await
            .unwrap_err();
        assert_eq!(refused(status), not_found);
        let status = world
            .api
            .resolve_outgoing(Request::new(ResolveOutgoingRequest {
                agent: Some(of(owner, agent)),
                action_id: id_bytes(action_id),
                approve: true,
            }))
            .await
            .unwrap_err();
        assert_eq!(refused(status), not_found);
    }
    // Nothing changed for the owner.
    let settings = world
        .messaging
        .agent_settings(world.owner, world.agent)
        .await;
    assert_eq!(settings, Ok(AgentSettings::default()));
    assert_eq!(
        world
            .messaging
            .waiting_outgoing(world.owner, world.agent)
            .await
            .map(|actions| actions.len()),
        Ok(1)
    );
}

#[tokio::test]
async fn the_owner_view_marks_conversations_an_agent_started() {
    let world = world().await;
    let me = of(world.owner, world.agent);
    world.messaging.register_user(world.owner).await.unwrap();
    let person = UserId::new();
    world.messaging.register_user(person).await.unwrap();
    world
        .messaging
        .set_outgoing_policy(
            world.owner,
            world.agent,
            OutgoingPolicyChange {
                requests: Some(push_domain::OutgoingPolicy::Allowed),
                chats: Some(push_domain::OutgoingPolicy::Allowed),
                ..OutgoingPolicyChange::default()
            },
        )
        .await
        .unwrap();
    world
        .messaging
        .contact_request(world.agent, person)
        .await
        .unwrap();
    world
        .messaging
        .contact_accept(person, world.agent)
        .await
        .unwrap();
    let started = world
        .messaging
        .open_direct(world.agent, person)
        .await
        .unwrap();
    let owners = world
        .messaging
        .open_direct(world.agent, world.owner)
        .await
        .unwrap();
    let listed = world
        .api
        .conversations(Request::new(me))
        .await
        .unwrap()
        .into_inner();
    let mark = |id: ConversationId| {
        listed
            .conversations
            .iter()
            .find(|conversation| conversation.conversation_id == id_bytes(id.0))
            .unwrap()
            .started_by_agent
    };
    assert!(mark(started));
    assert!(!mark(owners));
}
