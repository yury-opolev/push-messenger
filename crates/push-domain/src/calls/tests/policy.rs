//! Who may call whom (spec D11, D12): `Messaging::call_permission`.

use crate::error::DomainError;
use crate::ids::UserId;
use crate::testing::{TestService, befriend, service, user};
use crate::validation::ValidationError;

async fn may_call(service: &TestService, caller: UserId, callee: UserId) -> bool {
    match service.call_permission(caller, callee).await {
        Ok(()) => true,
        Err(DomainError::NotContact) => false,
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn calls_policy_contacts_may_call() {
    let (service, notifier) = service();
    let (a, b) = (user(&service).await, user(&service).await);
    befriend(&service, a, b).await;
    notifier.take();
    assert!(may_call(&service, a, b).await);
    assert!(may_call(&service, b, a).await);
    assert!(notifier.take().is_empty(), "publishes nothing");
    assert_eq!(
        service.call_permission(a, a).await,
        Err(DomainError::Invalid(ValidationError::SelfTarget))
    );
}

#[tokio::test]
async fn calls_policy_pending_or_strangers_may_not() {
    let (service, _) = service();
    let (a, b, c) = (
        user(&service).await,
        user(&service).await,
        user(&service).await,
    );
    service.contact_request(a, b).await.unwrap();
    assert!(!may_call(&service, a, b).await);
    assert!(!may_call(&service, b, a).await);
    assert!(!may_call(&service, a, c).await);
    assert!(!may_call(&service, c, a).await);
}

#[tokio::test]
async fn calls_policy_blocks_either_way_refuse() {
    let (service, _) = service();
    let (a, b, c) = (
        user(&service).await,
        user(&service).await,
        user(&service).await,
    );
    befriend(&service, a, b).await;
    befriend(&service, a, c).await;
    service.contact_block(a, b).await.unwrap();
    service.contact_block(c, a).await.unwrap();
    for (x, y) in [(a, b), (b, a), (a, c), (c, a)] {
        assert!(!may_call(&service, x, y).await);
    }
}

#[tokio::test]
async fn calls_policy_unknown_callee_looks_like_a_stranger() {
    let (service, _) = service();
    let a = user(&service).await;
    assert_eq!(
        service.call_permission(a, UserId::new()).await,
        Err(DomainError::NotContact)
    );
    assert_eq!(
        service.call_permission(UserId::new(), a).await,
        Err(DomainError::NotContact)
    );
}

#[tokio::test]
async fn agent_calls_only_its_owner() {
    let (service, _) = service();
    let (owner, agent) = (UserId::new(), UserId::new());
    service.enroll_agent(owner, agent).await.unwrap();
    let friend = user(&service).await;
    befriend(&service, agent, friend).await;
    let other_owner = user(&service).await;
    let other_agent = UserId::new();
    service
        .enroll_agent(other_owner, other_agent)
        .await
        .unwrap();
    assert!(may_call(&service, agent, owner).await);
    assert!(!may_call(&service, agent, friend).await);
    assert!(!may_call(&service, agent, other_owner).await);
    assert!(!may_call(&service, agent, other_agent).await);
}

#[tokio::test]
async fn only_the_owner_calls_the_agent() {
    let (service, _) = service();
    let (owner, agent) = (UserId::new(), UserId::new());
    service.enroll_agent(owner, agent).await.unwrap();
    let friend = user(&service).await;
    // Agents do not accept requests themselves: the agent asks, the friend accepts.
    befriend(&service, agent, friend).await;
    befriend(&service, friend, owner).await;
    assert!(may_call(&service, owner, agent).await);
    assert!(!may_call(&service, friend, agent).await);
    assert!(may_call(&service, friend, owner).await);
}

#[tokio::test]
async fn owner_and_agent_without_contact_rows_may_call() {
    let (service, _) = service();
    let (owner, agent) = (UserId::new(), UserId::new());
    service.enroll_agent(owner, agent).await.unwrap();
    // A block and an unblock remove both contact rows.
    service.contact_block(owner, agent).await.unwrap();
    assert!(!may_call(&service, owner, agent).await);
    assert!(!may_call(&service, agent, owner).await);
    service.contact_unblock(owner, agent).await.unwrap();
    assert_eq!(service.contact_state(owner, agent).await, Ok(None));
    assert_eq!(service.contact_state(agent, owner).await, Ok(None));
    assert!(may_call(&service, owner, agent).await);
    assert!(may_call(&service, agent, owner).await);
    // An agent cannot block its owner (spec 2026-11-06 agent connections D4).
    assert_eq!(
        service.contact_block(agent, owner).await,
        Err(DomainError::NotAllowed)
    );
    assert!(may_call(&service, agent, owner).await);
}

#[tokio::test]
async fn agents_never_call_each_other() {
    let (service, _) = service();
    let (owner, agent, sibling) = (UserId::new(), UserId::new(), UserId::new());
    service.enroll_agent(owner, agent).await.unwrap();
    service.enroll_agent(owner, sibling).await.unwrap();
    let other_owner = user(&service).await;
    let stranger_agent = UserId::new();
    service
        .enroll_agent(other_owner, stranger_agent)
        .await
        .unwrap();
    // Contacts through the owner's policy: still messages and cards only.
    service
        .set_agent_policy(owner, sibling, crate::model::AgentPolicy::AutoOwn)
        .await
        .unwrap();
    service.contact_request(agent, sibling).await.unwrap();
    for (caller, callee) in [(agent, sibling), (sibling, agent)] {
        assert_eq!(
            service.call_permission(caller, callee).await,
            Err(DomainError::NotAllowed)
        );
    }
    // Agents that are not contacts are strangers, as anyone else.
    assert_eq!(
        service.call_permission(agent, stranger_agent).await,
        Err(DomainError::NotContact)
    );
    assert!(may_call(&service, owner, agent).await);
    assert!(may_call(&service, sibling, owner).await);
}
