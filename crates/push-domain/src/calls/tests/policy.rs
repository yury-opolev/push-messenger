//! Who may call whom (spec D11, D12): `Messaging::call_permission`.

use std::sync::Arc;

use chrono::{TimeDelta, Utc};

use crate::clock::ManualClock;
use crate::error::DomainError;
use crate::ids::UserId;
use crate::memory::InMemoryStore;
use crate::model::{OutgoingKind, OutgoingPolicy, OutgoingPolicyChange, OutgoingState};
use crate::notify::{Event, RecordingNotifier};
use crate::outgoing::{CALL_GRANT, CALLS_PER_DAY};
use crate::service::Messaging;
use crate::testing::{TestService, allow_requests, befriend, service, user};
use crate::validation::ValidationError;

async fn may_call(service: &TestService, caller: UserId, callee: UserId) -> bool {
    match service.call_permission(caller, callee).await {
        Ok(_) => true,
        Err(DomainError::NotContact) => false,
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

type ClockedService = Messaging<InMemoryStore, Arc<ManualClock>, Arc<RecordingNotifier>>;

/// A service on a manual clock, an owner, its agent (requests allowed) and a person who
/// is the agent's accepted contact, with the agent's call policy `calls`.
async fn agent_with_contact(
    calls: Option<OutgoingPolicy>,
) -> (
    ClockedService,
    Arc<ManualClock>,
    Arc<RecordingNotifier>,
    UserId,
    UserId,
    UserId,
) {
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let notifier = Arc::new(RecordingNotifier::default());
    let service = Messaging::new(
        InMemoryStore::new(),
        Arc::clone(&clock),
        Arc::clone(&notifier),
    );
    let (owner, agent, friend) = (UserId::new(), UserId::new(), UserId::new());
    service.register_user(owner).await.unwrap();
    service.register_user(friend).await.unwrap();
    service.enroll_agent(owner, agent).await.unwrap();
    let change = OutgoingPolicyChange {
        requests: Some(OutgoingPolicy::Allowed),
        calls,
        ..OutgoingPolicyChange::default()
    };
    service
        .set_outgoing_policy(owner, agent, change)
        .await
        .unwrap();
    service.contact_request(agent, friend).await.unwrap();
    service.contact_accept(friend, agent).await.unwrap();
    notifier.take();
    (service, clock, notifier, owner, agent, friend)
}

fn waiting_id(answer: Result<bool, DomainError>) -> uuid::Uuid {
    match answer {
        Err(DomainError::WaitingForOwner { action_id }) => action_id,
        other => panic!("not waiting: {other:?}"),
    }
}

#[tokio::test]
async fn agent_calls_a_contact_under_off_answers_not_allowed_by_owner() {
    let (service, _, notifier, owner, agent, friend) = agent_with_contact(None).await;
    assert_eq!(
        service.call_permission(agent, friend).await,
        Err(DomainError::NotAllowedByOwner)
    );
    assert_eq!(service.waiting_outgoing(owner, agent).await, Ok(vec![]));
    assert!(notifier.take().is_empty());
    // Only the owner calls an agent, whatever the agent's own policy.
    assert_eq!(
        service.call_permission(friend, agent).await,
        Err(DomainError::NotContact)
    );
}

#[tokio::test]
async fn agent_calls_a_contact_under_ask_waits_for_the_owner() {
    let (service, _, notifier, owner, agent, friend) =
        agent_with_contact(Some(OutgoingPolicy::Ask)).await;
    let first = waiting_id(service.call_permission(agent, friend).await);
    let waiting = service.waiting_outgoing(owner, agent).await.unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].action_id, first);
    assert_eq!(waiting[0].kind, OutgoingKind::Call);
    assert_eq!(waiting[0].target, Some(friend));
    let events = notifier.take();
    assert!(
        matches!(&events[..], [Event::Outgoing { to, action }] if *to == owner && action.action_id == first),
        "{events:?}"
    );
    // Asked again while it waits: the same action, nobody told again.
    assert_eq!(
        waiting_id(service.call_permission(agent, friend).await),
        first
    );
    assert!(notifier.take().is_empty());
}

#[tokio::test]
async fn agent_calls_a_contact_under_allowed_counted_per_day() {
    let (service, _, _, _, agent, friend) = agent_with_contact(Some(OutgoingPolicy::Allowed)).await;
    for _ in 0..CALLS_PER_DAY {
        assert_eq!(service.call_permission(agent, friend).await, Ok(true));
    }
    assert!(matches!(
        service.call_permission(agent, friend).await,
        Err(DomainError::RateLimited { retry_after_s }) if retry_after_s > 0
    ));
}

#[tokio::test]
async fn an_approved_call_lets_the_agent_ring_once() {
    let (service, clock, _, owner, agent, friend) =
        agent_with_contact(Some(OutgoingPolicy::Ask)).await;
    let action = waiting_id(service.call_permission(agent, friend).await);
    let approved = service
        .approve_outgoing(owner, agent, action)
        .await
        .unwrap();
    assert_eq!(approved.state, OutgoingState::Approved);
    clock.advance(TimeDelta::minutes(14));
    assert_eq!(service.call_permission(agent, friend).await, Ok(true));
    // Used: the next call asks again, as a new action.
    let again = waiting_id(service.call_permission(agent, friend).await);
    assert_ne!(again, action);
}

#[tokio::test]
async fn an_approved_call_older_than_the_grant_is_not_used() {
    let (service, clock, _, owner, agent, friend) =
        agent_with_contact(Some(OutgoingPolicy::Ask)).await;
    let action = waiting_id(service.call_permission(agent, friend).await);
    service
        .approve_outgoing(owner, agent, action)
        .await
        .unwrap();
    clock.advance(CALL_GRANT + TimeDelta::seconds(1));
    let again = waiting_id(service.call_permission(agent, friend).await);
    assert_ne!(again, action);
}

#[tokio::test]
async fn agent_calls_its_owner_without_settings_marked_as_by_agent() {
    let (service, _) = service();
    let (owner, agent) = (user(&service).await, UserId::new());
    service.enroll_agent(owner, agent).await.unwrap();
    assert_eq!(service.call_permission(agent, owner).await, Ok(true));
    assert_eq!(service.call_permission(owner, agent).await, Ok(false));
    let (a, b) = (user(&service).await, user(&service).await);
    befriend(&service, a, b).await;
    assert_eq!(service.call_permission(a, b).await, Ok(false));
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
    allow_requests(&service, owner, agent).await;
    let friend = user(&service).await;
    befriend(&service, agent, friend).await;
    let other_owner = user(&service).await;
    let other_agent = UserId::new();
    service
        .enroll_agent(other_owner, other_agent)
        .await
        .unwrap();
    assert!(may_call(&service, agent, owner).await);
    // A contact who is a person: the owner's call policy decides (off by default).
    assert_eq!(
        service.call_permission(agent, friend).await,
        Err(DomainError::NotAllowedByOwner)
    );
    assert!(!may_call(&service, agent, other_owner).await);
    assert!(!may_call(&service, agent, other_agent).await);
}

#[tokio::test]
async fn only_the_owner_calls_the_agent() {
    let (service, _) = service();
    let (owner, agent) = (UserId::new(), UserId::new());
    service.enroll_agent(owner, agent).await.unwrap();
    allow_requests(&service, owner, agent).await;
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
    allow_requests(&service, owner, agent).await;
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
