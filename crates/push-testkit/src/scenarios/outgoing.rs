//! Proactive agents (spec 2026-11-09 proactive agents): the owner's outgoing policy on
//! an agent's contact requests, new chats and groups, the ask queue and its approvals,
//! declines and expiry, the limits per kind and the settings themselves.

use chrono::TimeDelta;
use push_domain::{
    AGENT_PENDING_OUT_MAX, AGENT_REQUESTS_PER_DAY, AGENT_REQUESTS_PER_HOUR, AgentPolicy,
    AgentSettings, CHATS_PER_DAY, Clock, ContactState, ConversationId, DomainError, DomainResult,
    Event, GROUPS_PER_DAY, INVITES_PER_DAY, InboundPolicy, OUTGOING_EXPIRY, OutgoingAction,
    OutgoingError, OutgoingKind, OutgoingPolicy, OutgoingPolicyChange, OutgoingState, OwnerAction,
    RESOLVED_KEPT, Store, Tx, UserId, WAITING_ACTIONS_MAX,
};

use uuid::Uuid;

use crate::world::{Person, World};
use crate::{Backend, Case, case_list};

/// An owner and one agent of theirs, with the enrollment's events taken.
async fn owned<B: Backend>(backend: &B) -> (World<B::Store>, Person, Person) {
    let world = World::new(backend).await;
    let owner = world.person().await;
    let agent = world.agent(&owner).await;
    world.notifier.take();
    (world, owner, agent)
}

/// Sets some of the agent's outgoing policies.
async fn set<S: Store + Clone>(
    world: &World<S>,
    owner: &Person,
    agent: &Person,
    change: OutgoingPolicyChange,
) {
    world
        .svc
        .set_outgoing_policy(owner.user, agent.user, change)
        .await
        .unwrap();
    world.notifier.take();
}

fn requests(policy: OutgoingPolicy) -> OutgoingPolicyChange {
    OutgoingPolicyChange {
        requests: Some(policy),
        ..OutgoingPolicyChange::default()
    }
}

fn chats(policy: OutgoingPolicy) -> OutgoingPolicyChange {
    OutgoingPolicyChange {
        chats: Some(policy),
        ..OutgoingPolicyChange::default()
    }
}

fn groups(policy: OutgoingPolicy) -> OutgoingPolicyChange {
    OutgoingPolicyChange {
        groups: Some(policy),
        ..OutgoingPolicyChange::default()
    }
}

/// A new person who is an accepted contact of the agent: they ask, the owner approves.
async fn contact<S: Store + Clone>(world: &World<S>, owner: &Person, agent: &Person) -> Person {
    let person = world.person().await;
    world
        .svc
        .contact_request(person.user, agent.user)
        .await
        .unwrap();
    world
        .svc
        .resolve_request(owner.user, agent.user, person.user, OwnerAction::Approve)
        .await
        .unwrap();
    world.notifier.take();
    person
}

/// The id of the action a result waits for.
fn waiting<T: std::fmt::Debug>(result: DomainResult<T>) -> Uuid {
    match result {
        Err(DomainError::WaitingForOwner { action_id }) => action_id,
        other => panic!("expected to wait for the owner, got {other:?}"),
    }
}

/// The outgoing action events, as `(to, action)`.
fn outgoing(events: &[Event]) -> Vec<(UserId, OutgoingAction)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Outgoing { to, action } => Some((*to, action.clone())),
            _ => None,
        })
        .collect()
}

async fn state<S: Store + Clone>(
    world: &World<S>,
    me: &Person,
    peer: &Person,
) -> Option<ContactState> {
    world.svc.contact_state(me.user, peer.user).await.unwrap()
}

async fn requests_ask_queues_and_owner_approval_sends_it<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let person = world.person().await;
    let id = waiting(world.svc.contact_request(agent.user, person.user).await);
    assert_eq!(state(&world, &agent, &person).await, None, "nothing sent");
    assert_eq!(state(&world, &person, &agent).await, None);
    let queued = OutgoingAction {
        action_id: id,
        agent: agent.user,
        kind: OutgoingKind::Request,
        target: Some(person.user),
        conversation: None,
        title: None,
        members: vec![],
        state: OutgoingState::Waiting,
        error: None,
        created_at: world.clock.now(),
        resolved_at: None,
        used_at: None,
    };
    assert_eq!(
        outgoing(&world.notifier.take()),
        [(owner.user, queued.clone())]
    );
    assert_eq!(
        world.svc.waiting_outgoing(owner.user, agent.user).await,
        Ok(vec![queued.clone()])
    );

    world.clock.advance(TimeDelta::minutes(5));
    let approved = world
        .svc
        .approve_outgoing(owner.user, agent.user, id)
        .await
        .unwrap();
    assert_eq!(
        approved,
        OutgoingAction {
            state: OutgoingState::Approved,
            resolved_at: Some(world.clock.now()),
            ..queued
        }
    );
    assert_eq!(
        state(&world, &agent, &person).await,
        Some(ContactState::RequestedOut)
    );
    assert_eq!(
        state(&world, &person, &agent).await,
        Some(ContactState::RequestedIn)
    );
    let events = world.notifier.take();
    assert_eq!(
        outgoing(&events),
        [
            (owner.user, approved.clone()),
            (agent.user, approved.clone())
        ]
    );
    assert!(events.contains(&Event::ContactChanged {
        user: person.user,
        peer: agent.user,
        state: Some(ContactState::RequestedIn),
    }));
    assert_eq!(
        world.svc.waiting_outgoing(owner.user, agent.user).await,
        Ok(vec![])
    );
    assert_eq!(
        world.svc.own_outgoing_actions(agent.user).await,
        Ok(vec![approved])
    );
    // Decided once.
    assert_eq!(
        world.svc.approve_outgoing(owner.user, agent.user, id).await,
        Err(DomainError::OutgoingNotFound)
    );
    assert_eq!(
        world.svc.decline_outgoing(owner.user, agent.user, id).await,
        Err(DomainError::OutgoingNotFound)
    );
}

async fn requests_off_answers_not_allowed_by_owner_and_stores_nothing<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set(&world, &owner, &agent, requests(OutgoingPolicy::Off)).await;
    let person = world.person().await;
    assert_eq!(
        world.svc.contact_request(agent.user, person.user).await,
        Err(DomainError::NotAllowedByOwner)
    );
    assert_eq!(world.notifier.take(), []);
    assert_eq!(state(&world, &person, &agent).await, None);
    assert_eq!(world.svc.own_outgoing_actions(agent.user).await, Ok(vec![]));
    let mut tx = world.store.begin().await.unwrap();
    assert_eq!(
        tx.agent_requests_since(agent.user, world.clock.now() - TimeDelta::days(1))
            .await,
        Ok((0, None)),
        "nothing counted"
    );
    tx.rollback().await.unwrap();
}

async fn requests_allowed_sends_at_once<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set(&world, &owner, &agent, requests(OutgoingPolicy::Allowed)).await;
    let person = world.person().await;
    assert_eq!(
        world.svc.contact_request(agent.user, person.user).await,
        Ok(ContactState::RequestedOut)
    );
    assert_eq!(outgoing(&world.notifier.take()), []);
    assert_eq!(
        state(&world, &person, &agent).await,
        Some(ContactState::RequestedIn)
    );
}

async fn same_waiting_action_answers_same_id<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let person = world.person().await;
    let first = waiting(world.svc.contact_request(agent.user, person.user).await);
    world.notifier.take();
    assert_eq!(
        waiting(world.svc.contact_request(agent.user, person.user).await),
        first
    );
    assert_eq!(world.notifier.take(), [], "the owner was told once");
    let other = world.person().await;
    assert_ne!(
        waiting(world.svc.contact_request(agent.user, other.user).await),
        first
    );
    assert_eq!(
        world
            .svc
            .waiting_outgoing(owner.user, agent.user)
            .await
            .unwrap()
            .len(),
        2
    );
}

async fn request_to_owner_is_never_gated<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set(&world, &owner, &agent, requests(OutgoingPolicy::Off)).await;
    assert_eq!(
        world.svc.contact_request(agent.user, owner.user).await,
        Ok(ContactState::Accepted)
    );
    // After the owner blocked and unblocked it, the agent may ask again; not counted.
    world
        .svc
        .contact_block(owner.user, agent.user)
        .await
        .unwrap();
    world
        .svc
        .contact_unblock(owner.user, agent.user)
        .await
        .unwrap();
    assert_eq!(
        world.svc.contact_request(agent.user, owner.user).await,
        Ok(ContactState::RequestedOut)
    );
    assert_eq!(outgoing(&world.notifier.take()), []);
}

async fn asking_back_a_waiting_requester_is_a_request_action<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let person = world.person().await;
    world
        .svc
        .contact_request(person.user, agent.user)
        .await
        .unwrap();
    assert_eq!(
        state(&world, &agent, &person).await,
        Some(ContactState::RequestedIn)
    );
    let id = waiting(world.svc.contact_request(agent.user, person.user).await);
    assert_eq!(
        state(&world, &agent, &person).await,
        Some(ContactState::RequestedIn),
        "the request still waits for the owner"
    );
    world
        .svc
        .approve_outgoing(owner.user, agent.user, id)
        .await
        .unwrap();
    assert_eq!(
        state(&world, &agent, &person).await,
        Some(ContactState::Accepted)
    );
    assert_eq!(
        state(&world, &person, &agent).await,
        Some(ContactState::Accepted)
    );
}

async fn gate_answer_is_the_same_for_unknown_blocked_and_present_targets<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let present = world.person().await;
    let blocker = world.person().await;
    world
        .svc
        .contact_block(blocker.user, agent.user)
        .await
        .unwrap();
    let unknown = UserId::new();
    world.notifier.take();
    let mut ids = Vec::new();
    for target in [unknown, blocker.user, present.user] {
        ids.push(waiting(world.svc.contact_request(agent.user, target).await));
        let events = outgoing(&world.notifier.take());
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].1.target, Some(target));
    }
    for id in ids {
        let action = world
            .svc
            .approve_outgoing(owner.user, agent.user, id)
            .await
            .unwrap();
        assert_eq!(action.state, OutgoingState::Approved, "{action:?}");
    }
    // Approved, each answers as an ordinary request: the block stays hidden.
    assert_eq!(
        state(&world, &agent, &blocker).await,
        Some(ContactState::RequestedOut)
    );
    assert_eq!(
        state(&world, &blocker, &agent).await,
        Some(ContactState::Blocked)
    );

    set(&world, &owner, &agent, requests(OutgoingPolicy::Off)).await;
    let other_blocker = world.person().await;
    world
        .svc
        .contact_block(other_blocker.user, agent.user)
        .await
        .unwrap();
    for target in [UserId::new(), other_blocker.user, world.person().await.user] {
        assert_eq!(
            world.svc.contact_request(agent.user, target).await,
            Err(DomainError::NotAllowedByOwner)
        );
    }
}

async fn chat_ask_then_approve_opens_it_started_by_agent<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let person = contact(&world, &owner, &agent).await;
    let id = waiting(world.svc.open_direct(agent.user, person.user).await);
    let events = outgoing(&world.notifier.take());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1.kind, OutgoingKind::Chat);
    assert!(
        world
            .svc
            .list_conversations(person.user, None)
            .await
            .unwrap()
            .is_empty(),
        "nothing opened yet"
    );

    let approved = world
        .svc
        .approve_outgoing(owner.user, agent.user, id)
        .await
        .unwrap();
    assert_eq!(approved.state, OutgoingState::Approved);
    let chat = approved.conversation.expect("the chat opened");
    assert!(world.conversation(chat).await.started_by_agent);
    let listed = world
        .svc
        .list_conversations(person.user, None)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].conversation_id, chat);
    assert!(listed[0].started_by_agent);
    // Now it exists: opening it again is not an action.
    assert_eq!(
        world.svc.open_direct(agent.user, person.user).await,
        Ok(chat)
    );
}

async fn existing_conversation_is_not_gated<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set(&world, &owner, &agent, chats(OutgoingPolicy::Off)).await;
    let person = contact(&world, &owner, &agent).await;
    let chat = world
        .svc
        .open_direct(person.user, agent.user)
        .await
        .unwrap();
    assert!(!world.conversation(chat).await.started_by_agent);
    assert_eq!(
        world.svc.open_direct(agent.user, person.user).await,
        Ok(chat)
    );
    // Replies in it are messages, not actions.
    world.send(&agent, chat, "hello").await;
    assert_eq!(outgoing(&world.notifier.take()), []);
}

async fn owner_chat_is_not_gated_and_not_started_by_agent<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set(&world, &owner, &agent, chats(OutgoingPolicy::Off)).await;
    let chat = world.svc.open_direct(agent.user, owner.user).await.unwrap();
    assert!(!world.conversation(chat).await.started_by_agent);
    assert_eq!(outgoing(&world.notifier.take()), []);
}

async fn group_off_by_default<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let person = contact(&world, &owner, &agent).await;
    assert_eq!(
        world
            .svc
            .create_group(agent.user, "Team", &[person.user])
            .await,
        Err(DomainError::NotAllowedByOwner)
    );
    // Off answers before the members are checked.
    assert_eq!(
        world
            .svc
            .create_group(agent.user, "Team", &[UserId::new()])
            .await,
        Err(DomainError::NotAllowedByOwner)
    );
    assert_eq!(world.notifier.take(), []);
}

async fn group_allowed_creates_started_by_agent<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set(&world, &owner, &agent, groups(OutgoingPolicy::Allowed)).await;
    let person = contact(&world, &owner, &agent).await;
    let group = world
        .svc
        .create_group(agent.user, "Team", &[person.user, owner.user])
        .await
        .unwrap();
    assert!(world.conversation(group).await.started_by_agent);
    assert_eq!(outgoing(&world.notifier.take()), []);
    // The rules still apply.
    assert_eq!(
        world
            .svc
            .create_group(agent.user, "Team", &[world.person().await.user])
            .await,
        Err(DomainError::NotContact)
    );
    // A person's group is not marked.
    let other = world.person().await;
    let theirs = world.group(&other, &[&person]).await;
    assert!(!world.conversation(theirs).await.started_by_agent);
}

async fn group_ask_then_approve_creates_it<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set(&world, &owner, &agent, groups(OutgoingPolicy::Ask)).await;
    let [first, second] = [
        contact(&world, &owner, &agent).await,
        contact(&world, &owner, &agent).await,
    ];
    let id = waiting(
        world
            .svc
            .create_group(
                agent.user,
                " Team ",
                &[second.user, first.user, second.user],
            )
            .await,
    );
    // The same group asked again, members in another order, is the same action.
    assert_eq!(
        waiting(
            world
                .svc
                .create_group(agent.user, "Team", &[first.user, second.user])
                .await
        ),
        id
    );
    let mut members = vec![first.user, second.user];
    members.sort();
    let queued = &world
        .svc
        .waiting_outgoing(owner.user, agent.user)
        .await
        .unwrap()[0];
    assert_eq!(queued.title.as_deref(), Some("Team"));
    assert_eq!(queued.members, members);

    let approved = world
        .svc
        .approve_outgoing(owner.user, agent.user, id)
        .await
        .unwrap();
    let group = approved.conversation.expect("the group was created");
    assert!(world.conversation(group).await.started_by_agent);
    assert!(world.member(group, &first).await.is_some());
    assert!(world.member(group, &second).await.is_some());
}

async fn invite_ask_then_approve_adds_member<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set(&world, &owner, &agent, groups(OutgoingPolicy::Allowed)).await;
    let [first, second] = [
        contact(&world, &owner, &agent).await,
        contact(&world, &owner, &agent).await,
    ];
    let group = world
        .svc
        .create_group(agent.user, "Team", &[first.user])
        .await
        .unwrap();
    set(&world, &owner, &agent, groups(OutgoingPolicy::Ask)).await;
    // Current members are no action.
    assert_eq!(
        world
            .svc
            .add_members(agent.user, group, &[first.user])
            .await,
        Ok(())
    );
    let id = waiting(
        world
            .svc
            .add_members(agent.user, group, &[second.user, first.user])
            .await,
    );
    let events = outgoing(&world.notifier.take());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1.kind, OutgoingKind::Invite);
    assert_eq!(events[0].1.conversation, Some(group));
    assert_eq!(events[0].1.members, [second.user]);
    assert!(world.member(group, &second).await.is_none());
    assert_eq!(
        waiting(
            world
                .svc
                .add_members(agent.user, group, &[second.user])
                .await
        ),
        id
    );

    let approved = world
        .svc
        .approve_outgoing(owner.user, agent.user, id)
        .await
        .unwrap();
    assert_eq!(approved.state, OutgoingState::Approved);
    assert_eq!(approved.conversation, Some(group));
    assert!(world.member(group, &second).await.unwrap().is_current());

    // Off refuses before anything else.
    set(&world, &owner, &agent, groups(OutgoingPolicy::Off)).await;
    assert_eq!(
        world
            .svc
            .add_members(agent.user, group, &[UserId::new()])
            .await,
        Err(DomainError::NotAllowedByOwner)
    );
}

async fn approval_failing_rules_ends_failed<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let person = contact(&world, &owner, &agent).await;
    let id = waiting(world.svc.open_direct(agent.user, person.user).await);
    world
        .svc
        .remove_agent_contact(owner.user, agent.user, person.user)
        .await
        .unwrap();
    world.notifier.take();
    let failed = world
        .svc
        .approve_outgoing(owner.user, agent.user, id)
        .await
        .unwrap();
    assert_eq!(failed.state, OutgoingState::Failed);
    assert_eq!(failed.error, Some(OutgoingError::NotContact));
    assert_eq!(failed.conversation, None);
    assert_eq!(
        outgoing(&world.notifier.take()),
        [(owner.user, failed.clone()), (agent.user, failed)]
    );
}

/// An approved request was counted against the hourly and daily limits when it was
/// queued, so its approval does not count it again; the caps on pending requests still
/// hold when it is carried out.
async fn approved_requests_keep_the_pending_caps<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let mut queued = Vec::new();
    for _ in 0..AGENT_REQUESTS_PER_HOUR {
        let person = world.person().await;
        let id = waiting(world.svc.contact_request(agent.user, person.user).await);
        queued.push((id, person));
    }
    let (last, rest) = queued.split_last().unwrap();
    for (id, person) in rest {
        let approved = world
            .svc
            .approve_outgoing(owner.user, agent.user, *id)
            .await
            .unwrap();
        assert_eq!(approved.state, OutgoingState::Approved);
        assert_eq!(
            state(&world, &agent, person).await,
            Some(ContactState::RequestedOut)
        );
    }

    let mut tx = world.store.begin().await.unwrap();
    for _ in 0..AGENT_PENDING_OUT_MAX {
        let peer = UserId::new();
        tx.insert_user(peer).await.unwrap();
        tx.put_contact(agent.user, peer, ContactState::RequestedOut)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    world.notifier.take();
    let (id, person) = last;
    let failed = world
        .svc
        .approve_outgoing(owner.user, agent.user, *id)
        .await
        .unwrap();
    assert_eq!(failed.state, OutgoingState::Failed);
    assert_eq!(failed.error, Some(OutgoingError::RateLimited));
    assert_eq!(state(&world, &agent, person).await, None);
    assert_eq!(state(&world, person, &agent).await, None);
    assert_eq!(
        outgoing(&world.notifier.take()),
        [(owner.user, failed.clone()), (agent.user, failed)]
    );
}

/// An invite the rules refuse on approval leaves the group as it was: no entry, no
/// `seq` taken.
async fn approval_failing_invite_leaves_the_group_unchanged<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set(&world, &owner, &agent, groups(OutgoingPolicy::Allowed)).await;
    let [first, second] = [
        contact(&world, &owner, &agent).await,
        contact(&world, &owner, &agent).await,
    ];
    let group = world
        .svc
        .create_group(agent.user, "Team", &[first.user])
        .await
        .unwrap();
    set(&world, &owner, &agent, groups(OutgoingPolicy::Ask)).await;
    let id = waiting(
        world
            .svc
            .add_members(agent.user, group, &[second.user])
            .await,
    );
    world
        .svc
        .remove_agent_contact(owner.user, agent.user, second.user)
        .await
        .unwrap();
    let before = world.conversation(group).await.last_seq;
    let failed = world
        .svc
        .approve_outgoing(owner.user, agent.user, id)
        .await
        .unwrap();
    assert_eq!(failed.state, OutgoingState::Failed);
    assert_eq!(failed.error, Some(OutgoingError::NotContact));
    assert_eq!(world.conversation(group).await.last_seq, before);
    assert!(world.member(group, &second).await.is_none());
    let sent = world.send(&agent, group, "still here").await;
    assert_eq!(sent.seq, before + 1);
}

async fn decline_and_expiry_tell_agent_owner_declined<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let person = world.person().await;
    let declined_id = waiting(world.svc.contact_request(agent.user, person.user).await);
    world.notifier.take();
    let declined = world
        .svc
        .decline_outgoing(owner.user, agent.user, declined_id)
        .await
        .unwrap();
    assert_eq!(declined.state, OutgoingState::Declined);
    assert_eq!(
        outgoing(&world.notifier.take()),
        [
            (owner.user, declined.clone()),
            (agent.user, declined.clone())
        ]
    );
    assert_eq!(state(&world, &person, &agent).await, None);
    assert_eq!(
        world
            .svc
            .decline_outgoing(owner.user, agent.user, declined_id)
            .await,
        Err(DomainError::OutgoingNotFound)
    );

    let other = world.person().await;
    world.clock.advance(TimeDelta::minutes(1));
    let expiring = waiting(world.svc.contact_request(agent.user, other.user).await);
    world.notifier.take();
    world.clock.advance(OUTGOING_EXPIRY - TimeDelta::seconds(1));
    world.svc.release_refusals().await.unwrap();
    assert_eq!(outgoing(&world.notifier.take()), [], "not yet");
    world.clock.advance(TimeDelta::seconds(1));
    world.svc.release_refusals().await.unwrap();
    let events = outgoing(&world.notifier.take());
    assert_eq!(events.len(), 2, "{events:?}");
    let expired = events[0].1.clone();
    assert_eq!(expired.action_id, expiring);
    assert_eq!(expired.state, OutgoingState::Expired);
    assert_eq!(expired.resolved_at, Some(world.clock.now()));
    assert_eq!(
        events,
        [(owner.user, expired.clone()), (agent.user, expired.clone())]
    );
    assert_eq!(
        world.svc.waiting_outgoing(owner.user, agent.user).await,
        Ok(vec![])
    );
    assert_eq!(
        world
            .svc
            .approve_outgoing(owner.user, agent.user, expiring)
            .await,
        Err(DomainError::OutgoingNotFound)
    );
    // The agent sees what was resolved in the last day (the decline is a minute older);
    // then it is gone.
    assert_eq!(
        world.svc.own_outgoing_actions(agent.user).await,
        Ok(vec![expired])
    );
    world.clock.advance(RESOLVED_KEPT + TimeDelta::seconds(1));
    world.svc.release_refusals().await.unwrap();
    assert_eq!(world.svc.own_outgoing_actions(agent.user).await, Ok(vec![]));
    let mut tx = world.store.begin().await.unwrap();
    assert_eq!(tx.outgoing(expiring).await, Ok(None), "pruned");
    assert_eq!(
        tx.agent_actions_since(
            agent.user,
            OutgoingKind::Chat,
            world.clock.now() - OUTGOING_EXPIRY * 3
        )
        .await,
        Ok((0, None))
    );
    tx.rollback().await.unwrap();
}

async fn limits_per_kind_and_waiting_cap<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    world
        .svc
        .set_agent_policy(owner.user, agent.user, AgentPolicy::AutoAny)
        .await
        .unwrap();
    let mut contacts = Vec::new();
    for _ in 0..=CHATS_PER_DAY {
        let person = world.person().await;
        world
            .svc
            .contact_request(person.user, agent.user)
            .await
            .unwrap();
        contacts.push(person);
    }
    world.notifier.take();

    // The waiting cap.
    let cap = WAITING_ACTIONS_MAX as usize;
    for person in &contacts[..cap] {
        waiting(world.svc.open_direct(agent.user, person.user).await);
    }
    assert_eq!(
        world.svc.open_direct(agent.user, contacts[cap].user).await,
        Err(DomainError::RateLimited { retry_after_s: 600 })
    );
    // Queued chats count: the rest of the day's chats are opened at once.
    set(&world, &owner, &agent, chats(OutgoingPolicy::Allowed)).await;
    let limit = CHATS_PER_DAY as usize;
    for person in &contacts[cap..limit] {
        world
            .svc
            .open_direct(agent.user, person.user)
            .await
            .unwrap();
    }
    assert_eq!(
        world
            .svc
            .open_direct(agent.user, contacts[limit].user)
            .await,
        Err(DomainError::RateLimited {
            retry_after_s: 86_400
        })
    );
    world.clock.advance(TimeDelta::hours(1));
    assert_eq!(
        world
            .svc
            .open_direct(agent.user, contacts[limit].user)
            .await,
        Err(DomainError::RateLimited {
            retry_after_s: 82_800
        })
    );

    // Members added count one each; groups count once.
    set(&world, &owner, &agent, groups(OutgoingPolicy::Allowed)).await;
    let group = world
        .svc
        .create_group(agent.user, "First", &[contacts[0].user])
        .await
        .unwrap();
    let invited: Vec<UserId> = contacts[1..=INVITES_PER_DAY as usize]
        .iter()
        .map(|person| person.user)
        .collect();
    world
        .svc
        .add_members(agent.user, group, &invited)
        .await
        .unwrap();
    assert!(matches!(
        world
            .svc
            .add_members(agent.user, group, &[contacts[30].user])
            .await,
        Err(DomainError::RateLimited { .. })
    ));
    for n in 1..GROUPS_PER_DAY {
        world
            .svc
            .create_group(agent.user, &format!("Group {n}"), &[contacts[0].user])
            .await
            .unwrap();
    }
    assert!(matches!(
        world
            .svc
            .create_group(agent.user, "One more", &[contacts[0].user])
            .await,
        Err(DomainError::RateLimited { .. })
    ));
}

async fn contact_request_limits_are_ten_an_hour_twenty_a_day<B: Backend>(backend: &B) {
    assert_eq!((AGENT_REQUESTS_PER_HOUR, AGENT_REQUESTS_PER_DAY), (10, 20));
    let (world, owner, agent) = owned(backend).await;
    set(&world, &owner, &agent, requests(OutgoingPolicy::Allowed)).await;
    for hour in 0..2 {
        for _ in 0..AGENT_REQUESTS_PER_HOUR {
            world
                .svc
                .contact_request(agent.user, UserId::new())
                .await
                .unwrap();
        }
        assert!(
            matches!(
                world.svc.contact_request(agent.user, UserId::new()).await,
                Err(DomainError::RateLimited { retry_after_s }) if retry_after_s <= 3600
            ),
            "hour {hour}"
        );
        world
            .clock
            .advance(TimeDelta::hours(1) + TimeDelta::seconds(1));
    }
    // Twenty in the day: the next waits for the first to be a day old.
    assert_eq!(
        world.svc.contact_request(agent.user, UserId::new()).await,
        Err(DomainError::RateLimited {
            retry_after_s: 24 * 3600 - 2 * 3601
        })
    );
    // Queued requests count the same way.
    set(&world, &owner, &agent, requests(OutgoingPolicy::Ask)).await;
    assert!(matches!(
        world.svc.contact_request(agent.user, UserId::new()).await,
        Err(DomainError::RateLimited { .. })
    ));
}

async fn settings_changes_publish_to_agent<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    assert_eq!(
        world.svc.agent_settings(owner.user, agent.user).await,
        Ok(AgentSettings::default())
    );
    let changed = AgentSettings {
        chats: OutgoingPolicy::Allowed,
        calls: OutgoingPolicy::Ask,
        ..AgentSettings::default()
    };
    assert_eq!(
        world
            .svc
            .set_outgoing_policy(
                owner.user,
                agent.user,
                OutgoingPolicyChange {
                    chats: Some(OutgoingPolicy::Allowed),
                    calls: Some(OutgoingPolicy::Ask),
                    ..OutgoingPolicyChange::default()
                }
            )
            .await,
        Ok(changed)
    );
    assert_eq!(
        world.notifier.take(),
        [Event::AgentSettings {
            agent: agent.user,
            settings: changed
        }]
    );
    let inbound = AgentSettings {
        inbound: InboundPolicy::Contacts,
        ..changed
    };
    assert_eq!(
        world
            .svc
            .set_inbound_policy(owner.user, agent.user, InboundPolicy::Contacts)
            .await,
        Ok(inbound)
    );
    assert_eq!(
        world.notifier.take(),
        [Event::AgentSettings {
            agent: agent.user,
            settings: inbound
        }]
    );
    assert_eq!(
        world.svc.agent_settings(owner.user, agent.user).await,
        Ok(inbound)
    );
    assert_eq!(world.svc.own_agent_settings(agent.user).await, Ok(inbound));

    // Only the owner decides; only agents read their own.
    let stranger = world.person().await;
    assert_eq!(
        world.svc.agent_settings(stranger.user, agent.user).await,
        Err(DomainError::AgentNotFound)
    );
    assert_eq!(
        world
            .svc
            .set_inbound_policy(agent.user, agent.user, InboundPolicy::OwnerOnly)
            .await,
        Err(DomainError::AgentNotFound)
    );
    assert_eq!(
        world.svc.waiting_outgoing(stranger.user, agent.user).await,
        Err(DomainError::AgentNotFound)
    );
    assert_eq!(
        world.svc.own_agent_settings(owner.user).await,
        Err(DomainError::NotAllowed)
    );
    assert_eq!(
        world.svc.own_outgoing_actions(owner.user).await,
        Err(DomainError::NotAllowed)
    );
}

async fn owners_decide_only_their_agents_actions<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let sibling = world.agent(&owner).await;
    let person = world.person().await;
    let id = waiting(world.svc.contact_request(agent.user, person.user).await);
    assert_eq!(
        world
            .svc
            .approve_outgoing(owner.user, sibling.user, id)
            .await,
        Err(DomainError::OutgoingNotFound)
    );
    let stranger = world.person().await;
    assert_eq!(
        world
            .svc
            .approve_outgoing(stranger.user, agent.user, id)
            .await,
        Err(DomainError::AgentNotFound)
    );
    assert_eq!(
        world
            .svc
            .decline_outgoing(owner.user, agent.user, Uuid::new_v4())
            .await,
        Err(DomainError::OutgoingNotFound)
    );
}

async fn deleting_agent_drops_its_actions<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let person = world.person().await;
    let id = waiting(world.svc.contact_request(agent.user, person.user).await);
    world
        .svc
        .set_inbound_policy(owner.user, agent.user, InboundPolicy::Contacts)
        .await
        .unwrap();
    world.svc.user_deleted(agent.user).await.unwrap();
    world.notifier.take();
    let mut tx = world.store.begin().await.unwrap();
    assert_eq!(tx.outgoing(id).await, Ok(None));
    assert_eq!(
        tx.agent_settings(agent.user).await,
        Ok(AgentSettings::default())
    );
    tx.rollback().await.unwrap();
    // Nothing is left to expire or tell.
    world.clock.advance(OUTGOING_EXPIRY * 2);
    world.svc.release_refusals().await.unwrap();
    assert_eq!(outgoing(&world.notifier.take()), []);
}

async fn people_are_not_gated<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let [alice, bob, carol] = [
        world.person().await,
        world.person().await,
        world.person().await,
    ];
    assert_eq!(
        world.svc.contact_request(alice.user, bob.user).await,
        Ok(ContactState::RequestedOut)
    );
    world
        .svc
        .contact_accept(bob.user, alice.user)
        .await
        .unwrap();
    let chat = world.svc.open_direct(alice.user, bob.user).await.unwrap();
    assert!(!world.conversation(chat).await.started_by_agent);
    world.befriend(&alice, &carol).await;
    let group: ConversationId = world
        .svc
        .create_group(alice.user, "Friends", &[bob.user])
        .await
        .unwrap();
    world
        .svc
        .add_members(alice.user, group, &[carol.user])
        .await
        .unwrap();
    assert_eq!(outgoing(&world.notifier.take()), []);
    assert_eq!(
        world.svc.own_agent_settings(alice.user).await,
        Err(DomainError::NotAllowed)
    );
}

pub(super) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        requests_ask_queues_and_owner_approval_sends_it,
        requests_off_answers_not_allowed_by_owner_and_stores_nothing,
        requests_allowed_sends_at_once,
        same_waiting_action_answers_same_id,
        request_to_owner_is_never_gated,
        asking_back_a_waiting_requester_is_a_request_action,
        gate_answer_is_the_same_for_unknown_blocked_and_present_targets,
        chat_ask_then_approve_opens_it_started_by_agent,
        existing_conversation_is_not_gated,
        owner_chat_is_not_gated_and_not_started_by_agent,
        group_off_by_default,
        group_allowed_creates_started_by_agent,
        group_ask_then_approve_creates_it,
        invite_ask_then_approve_adds_member,
        approval_failing_rules_ends_failed,
        approved_requests_keep_the_pending_caps,
        approval_failing_invite_leaves_the_group_unchanged,
        decline_and_expiry_tell_agent_owner_declined,
        limits_per_kind_and_waiting_cap,
        contact_request_limits_are_ten_an_hour_twenty_a_day,
        settings_changes_publish_to_agent,
        owners_decide_only_their_agents_actions,
        deleting_agent_drops_its_actions,
        people_are_not_gated,
    ]
}
