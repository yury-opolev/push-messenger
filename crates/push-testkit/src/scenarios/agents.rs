//! Agent connections (spec 2026-11-06 agent connections and the owner view): every policy
//! against people and agents, the owner's actions, the agents' own rules and limits.

use chrono::TimeDelta;
use push_domain::{
    AGENT_REQUESTS_PER_HOUR, AGENT_WAITING_OUT_MAX, AgentPolicy, Clock, ContactState,
    ContactSummary, DomainError, Event, InviteRecord, OwnerAction, Store, Tx, UserId,
};

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

async fn state<S: Store + Clone>(
    world: &World<S>,
    me: &Person,
    peer: &Person,
) -> Option<ContactState> {
    world.svc.contact_state(me.user, peer.user).await.unwrap()
}

async fn set_policy<S: Store + Clone>(
    world: &World<S>,
    owner: &Person,
    agent: &Person,
    policy: AgentPolicy,
) {
    assert_eq!(
        world
            .svc
            .set_agent_policy(owner.user, agent.user, policy)
            .await,
        Ok(policy)
    );
}

fn waiting_event(owner: &Person, agent: &Person, user: &Person) -> Event {
    Event::AgentContact {
        owner: owner.user,
        agent: agent.user,
        user: user.user,
        state: Some(ContactState::RequestedIn),
    }
}

async fn waiting<S: Store + Clone>(
    world: &World<S>,
    owner: &Person,
    agent: &Person,
) -> Vec<UserId> {
    world
        .svc
        .waiting_requests(owner.user, agent.user)
        .await
        .unwrap()
        .into_iter()
        .map(|(user, _)| user)
        .collect()
}

async fn ask_holds_requests_from_people_and_agents_for_the_owner<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    assert_eq!(
        world.svc.agent_policy(owner.user, agent.user).await,
        Ok(AgentPolicy::Ask)
    );
    let person = world.person().await;
    assert_eq!(
        world.svc.contact_request(person.user, agent.user).await,
        Ok(ContactState::RequestedOut)
    );
    assert_eq!(
        state(&world, &agent, &person).await,
        Some(ContactState::RequestedIn)
    );
    let events = world.notifier.take();
    assert!(
        events.contains(&waiting_event(&owner, &agent, &person)),
        "{events:?}"
    );

    let other_owner = world.person().await;
    let other_agent = world.agent(&other_owner).await;
    world.notifier.take();
    assert_eq!(
        world
            .svc
            .contact_request(other_agent.user, agent.user)
            .await,
        Ok(ContactState::RequestedOut)
    );
    assert!(
        world
            .notifier
            .take()
            .contains(&waiting_event(&owner, &agent, &other_agent))
    );
    assert_eq!(
        waiting(&world, &owner, &agent).await,
        [person.user, other_agent.user]
    );
    // A repeated request changes nothing.
    assert_eq!(
        world.svc.contact_request(person.user, agent.user).await,
        Ok(ContactState::RequestedOut)
    );
    assert_eq!(world.notifier.take(), []);
}

async fn the_owner_approves_declines_or_blocks_waiting_requests<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let [first, second, third] = [
        world.person().await,
        world.person().await,
        world.person().await,
    ];
    for person in [&first, &second, &third] {
        world
            .svc
            .contact_request(person.user, agent.user)
            .await
            .unwrap();
    }
    world.notifier.take();

    world
        .svc
        .resolve_request(owner.user, agent.user, first.user, OwnerAction::Approve)
        .await
        .unwrap();
    assert_eq!(
        state(&world, &agent, &first).await,
        Some(ContactState::Accepted)
    );
    assert_eq!(
        state(&world, &first, &agent).await,
        Some(ContactState::Accepted)
    );
    assert!(world.notifier.take().contains(&Event::AgentContact {
        owner: owner.user,
        agent: agent.user,
        user: first.user,
        state: Some(ContactState::Accepted),
    }));

    world
        .svc
        .resolve_request(owner.user, agent.user, second.user, OwnerAction::Decline)
        .await
        .unwrap();
    assert_eq!(state(&world, &agent, &second).await, None);
    assert_eq!(state(&world, &second, &agent).await, None);
    // As any decline: the requester hears nothing.
    let events = world.notifier.take();
    assert!(
        events.iter().all(|event| !matches!(
            event,
            Event::ContactChanged { user, .. } if *user == second.user
        )),
        "{events:?}"
    );

    world
        .svc
        .resolve_request(owner.user, agent.user, third.user, OwnerAction::Block)
        .await
        .unwrap();
    assert_eq!(
        state(&world, &agent, &third).await,
        Some(ContactState::Blocked)
    );
    assert_eq!(state(&world, &third, &agent).await, None);
    assert!(waiting(&world, &owner, &agent).await.is_empty());

    assert_eq!(
        world
            .svc
            .resolve_request(owner.user, agent.user, second.user, OwnerAction::Approve)
            .await,
        Err(DomainError::NoPendingRequest)
    );
    assert_eq!(
        world
            .svc
            .resolve_request(owner.user, agent.user, owner.user, OwnerAction::Block)
            .await,
        Err(DomainError::NotAllowed)
    );
}

async fn agents_cannot_decide_on_their_requests<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let person = world.person().await;
    world
        .svc
        .contact_request(person.user, agent.user)
        .await
        .unwrap();
    assert_eq!(
        world.svc.contact_accept(agent.user, person.user).await,
        Err(DomainError::OwnerDecides)
    );
    assert_eq!(
        world.svc.contact_decline(agent.user, person.user).await,
        Err(DomainError::OwnerDecides)
    );
    assert_eq!(
        world.svc.contact_block(agent.user, person.user).await,
        Err(DomainError::OwnerDecides)
    );
    assert_eq!(
        world.svc.contact_block(agent.user, owner.user).await,
        Err(DomainError::NotAllowed)
    );
    assert_eq!(
        state(&world, &agent, &person).await,
        Some(ContactState::RequestedIn)
    );
    // Blocking anyone else stays the agent's right.
    let stranger = world.person().await;
    assert_eq!(
        world.svc.contact_block(agent.user, stranger.user).await,
        Ok(())
    );
    // People still decide for themselves.
    let other = world.person().await;
    world
        .svc
        .contact_request(other.user, person.user)
        .await
        .unwrap();
    assert_eq!(
        world.svc.contact_accept(person.user, other.user).await,
        Ok(())
    );
}

async fn auto_own_accepts_the_owners_agents_only<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set_policy(&world, &owner, &agent, AgentPolicy::AutoOwn).await;
    let sibling = world.agent(&owner).await;
    assert_eq!(
        world.svc.contact_request(sibling.user, agent.user).await,
        Ok(ContactState::Accepted)
    );
    assert_eq!(
        state(&world, &agent, &sibling).await,
        Some(ContactState::Accepted)
    );
    let person = world.person().await;
    let foreign_owner = world.person().await;
    let foreign = world.agent(&foreign_owner).await;
    for requester in [&person, &foreign] {
        assert_eq!(
            world.svc.contact_request(requester.user, agent.user).await,
            Ok(ContactState::RequestedOut)
        );
    }
    assert_eq!(
        waiting(&world, &owner, &agent).await.len(),
        2,
        "people and other owners' agents wait"
    );
}

async fn auto_owner_contacts_accepts_the_owners_circle<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set_policy(&world, &owner, &agent, AgentPolicy::AutoOwnerContacts).await;
    let friend = world.person().await;
    world.befriend(&owner, &friend).await;
    let friends_agent = world.agent(&friend).await;
    let sibling = world.agent(&owner).await;
    for requester in [&friend, &friends_agent, &sibling] {
        assert_eq!(
            world.svc.contact_request(requester.user, agent.user).await,
            Ok(ContactState::Accepted),
            "the owner's circle is accepted"
        );
    }
    // A pending request to the owner is not a contact yet.
    let asking = world.person().await;
    world
        .svc
        .contact_request(asking.user, owner.user)
        .await
        .unwrap();
    let stranger = world.person().await;
    for requester in [&asking, &stranger] {
        assert_eq!(
            world.svc.contact_request(requester.user, agent.user).await,
            Ok(ContactState::RequestedOut)
        );
    }
    assert_eq!(waiting(&world, &owner, &agent).await.len(), 2);
}

async fn auto_any_accepts_everyone<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set_policy(&world, &owner, &agent, AgentPolicy::AutoAny).await;
    let person = world.person().await;
    let other_owner = world.person().await;
    let foreign = world.agent(&other_owner).await;
    for requester in [&person, &foreign] {
        assert_eq!(
            world.svc.contact_request(requester.user, agent.user).await,
            Ok(ContactState::Accepted)
        );
        assert_eq!(
            state(&world, &agent, requester).await,
            Some(ContactState::Accepted)
        );
    }
}

async fn refuse_all_looks_like_a_decline_to_the_requester<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set_policy(&world, &owner, &agent, AgentPolicy::RefuseAll).await;
    let other_owner = world.person().await;
    let foreign = world.agent(&other_owner).await;
    let person = world.person().await;
    world.notifier.take();
    for requester in [&person, &foreign] {
        assert_eq!(
            world.svc.contact_request(requester.user, agent.user).await,
            Ok(ContactState::RequestedOut),
            "the answer of any pending request"
        );
        assert_eq!(
            state(&world, requester, &agent).await,
            Some(ContactState::RequestedOut)
        );
        assert_eq!(state(&world, &agent, requester).await, None);
        // The requester hears of its own request, as always (and so does the requester's
        // own owner, for an agent); the agent and its owner hear nothing.
        let events = world.notifier.take();
        assert!(events.contains(&Event::ContactChanged {
            user: requester.user,
            peer: agent.user,
            state: Some(ContactState::RequestedOut),
        }));
        assert!(
            events.iter().all(|event| match event {
                Event::ContactChanged { user, .. } => *user == requester.user,
                Event::AgentContact { owner: to, .. } => *to == other_owner.user,
                _ => false,
            }),
            "{events:?}"
        );
    }
    assert!(waiting(&world, &owner, &agent).await.is_empty());
    assert_eq!(world.svc.release_refusals().await, Ok(0), "not due yet");

    // After the random delay (at most six hours by default) the pending request ends
    // without an event, exactly as a decline ends it for the requester.
    world
        .clock
        .advance(TimeDelta::hours(6) + TimeDelta::seconds(1));
    assert_eq!(world.svc.release_refusals().await, Ok(2));
    for requester in [&person, &foreign] {
        assert_eq!(state(&world, requester, &agent).await, None);
    }
    assert_eq!(world.notifier.take(), []);

    // The owner is never refused.
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
        world.svc.contact_request(owner.user, agent.user).await,
        Ok(ContactState::Accepted)
    );
}

async fn a_refusal_ends_early_when_the_agent_asks_back<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set_policy(&world, &owner, &agent, AgentPolicy::RefuseAll).await;
    let person = world.person().await;
    world
        .svc
        .contact_request(person.user, agent.user)
        .await
        .unwrap();
    assert_eq!(
        world.svc.contact_request(agent.user, person.user).await,
        Ok(ContactState::Accepted)
    );
    world.clock.advance(TimeDelta::days(1));
    assert_eq!(world.svc.release_refusals().await, Ok(0));
    assert_eq!(
        state(&world, &person, &agent).await,
        Some(ContactState::Accepted)
    );
}

async fn waiting_requests_survive_a_policy_change<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let person = world.person().await;
    world
        .svc
        .contact_request(person.user, agent.user)
        .await
        .unwrap();
    set_policy(&world, &owner, &agent, AgentPolicy::AutoAny).await;
    assert_eq!(waiting(&world, &owner, &agent).await, [person.user]);
    assert_eq!(
        world.svc.agent_policy(owner.user, agent.user).await,
        Ok(AgentPolicy::AutoAny)
    );
}

async fn agents_requests_are_limited<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    // Requests to ids that are not users count too: they would be free probes otherwise.
    for _ in 0..AGENT_REQUESTS_PER_HOUR {
        assert_eq!(
            world.svc.contact_request(agent.user, UserId::new()).await,
            Ok(ContactState::RequestedOut)
        );
    }
    let person = world.person().await;
    match world.svc.contact_request(agent.user, person.user).await {
        Err(DomainError::RateLimited { retry_after_s }) => {
            assert!((1..=3600).contains(&retry_after_s), "{retry_after_s}");
        }
        other => panic!("expected a rate limit, got {other:?}"),
    }
    assert_eq!(state(&world, &person, &agent).await, None, "nothing stored");
    world
        .clock
        .advance(TimeDelta::hours(1) + TimeDelta::seconds(1));
    assert_eq!(
        world.svc.contact_request(agent.user, person.user).await,
        Ok(ContactState::RequestedOut)
    );
    // People are not limited here (the gateway limits everyone).
    let busy = world.person().await;
    for _ in 0..AGENT_REQUESTS_PER_HOUR + 5 {
        world
            .svc
            .contact_request(busy.user, UserId::new())
            .await
            .unwrap();
    }
    let _ = owner;
}

async fn agents_cannot_flood_other_owners<B: Backend>(backend: &B) {
    let (world, _owner, agent) = owned(backend).await;
    for _ in 0..AGENT_WAITING_OUT_MAX {
        let other_owner = world.person().await;
        let target = world.agent(&other_owner).await;
        assert_eq!(
            world.svc.contact_request(agent.user, target.user).await,
            Ok(ContactState::RequestedOut)
        );
    }
    // The cap counts the agent's requests that wait for owners, whatever the next target.
    let person = world.person().await;
    assert_eq!(
        world.svc.contact_request(agent.user, person.user).await,
        Err(DomainError::RateLimited { retry_after_s: 600 })
    );
}

async fn agents_cannot_make_contacts_through_invites<B: Backend>(backend: &B) {
    let (world, _owner, agent) = owned(backend).await;
    assert_eq!(
        world
            .svc
            .create_invite(agent.user, TimeDelta::hours(1), 1)
            .await
            .map(|_| ()),
        Err(DomainError::NotAllowed)
    );
    // An invite an agent created before is void.
    let mut tx = world.store.begin().await.unwrap();
    tx.insert_invite(InviteRecord {
        code: "ABCDEFGHJK".into(),
        user_id: agent.user,
        expires_at: world.clock.now() + TimeDelta::hours(1),
        max_uses: 1,
        uses: 0,
        revoked_at: None,
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let person = world.person().await;
    assert_eq!(
        world.svc.redeem_invite(person.user, "ABCDEFGHJK").await,
        Err(DomainError::InviteNotFound)
    );
}

async fn the_owner_lists_and_removes_contacts<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    set_policy(&world, &owner, &agent, AgentPolicy::AutoAny).await;
    let friend = world.person().await;
    world
        .svc
        .contact_request(friend.user, agent.user)
        .await
        .unwrap();
    let asked = world.person().await;
    world
        .svc
        .contact_request(agent.user, asked.user)
        .await
        .unwrap();
    let mut listed = world
        .svc
        .agent_contacts(owner.user, agent.user)
        .await
        .unwrap();
    listed.sort_by_key(|contact| contact.user_id);
    let mut expected = vec![
        ContactSummary {
            user_id: owner.user,
            state: ContactState::Accepted,
        },
        ContactSummary {
            user_id: friend.user,
            state: ContactState::Accepted,
        },
        ContactSummary {
            user_id: asked.user,
            state: ContactState::RequestedOut,
        },
    ];
    expected.sort_by_key(|contact| contact.user_id);
    assert_eq!(listed, expected);
    world.notifier.take();

    world
        .svc
        .remove_agent_contact(owner.user, agent.user, friend.user)
        .await
        .unwrap();
    assert_eq!(state(&world, &agent, &friend).await, None);
    assert_eq!(state(&world, &friend, &agent).await, None);
    let events = world.notifier.take();
    for (user, peer) in [(agent.user, friend.user), (friend.user, agent.user)] {
        assert!(
            events.contains(&Event::ContactChanged {
                user,
                peer,
                state: None
            }),
            "{events:?}"
        );
    }
    world
        .svc
        .remove_agent_contact(owner.user, agent.user, asked.user)
        .await
        .unwrap();
    assert_eq!(state(&world, &asked, &agent).await, None, "withdrawn");
    assert_eq!(
        world
            .svc
            .remove_agent_contact(owner.user, agent.user, asked.user)
            .await,
        Ok(()),
        "idempotent"
    );
    assert_eq!(
        world
            .svc
            .remove_agent_contact(owner.user, agent.user, owner.user)
            .await,
        Err(DomainError::NotAllowed)
    );
    assert_eq!(
        state(&world, &agent, &owner).await,
        Some(ContactState::Accepted)
    );
}

async fn only_the_owner_reaches_the_agent<B: Backend>(backend: &B) {
    let (world, owner, agent) = owned(backend).await;
    let contact = world.person().await;
    set_policy(&world, &owner, &agent, AgentPolicy::AutoAny).await;
    world
        .svc
        .contact_request(contact.user, agent.user)
        .await
        .unwrap();
    let stranger = world.person().await;
    let other_owner = world.person().await;
    let other_agent = world.agent(&other_owner).await;
    for (caller, target) in [
        (&stranger, &agent),
        (&contact, &agent),
        (&agent, &agent),
        (&other_owner, &agent),
        (&owner, &other_agent),
        (&owner, &contact),
        (&owner, &owner),
    ] {
        assert_eq!(
            world.svc.agent_policy(caller.user, target.user).await,
            Err(DomainError::AgentNotFound)
        );
        assert_eq!(
            world.svc.waiting_requests(caller.user, target.user).await,
            Err(DomainError::AgentNotFound)
        );
        assert_eq!(
            world
                .svc
                .set_agent_policy(caller.user, target.user, AgentPolicy::RefuseAll)
                .await,
            Err(DomainError::AgentNotFound)
        );
        assert_eq!(
            world.svc.agent_contacts(caller.user, target.user).await,
            Err(DomainError::AgentNotFound)
        );
        assert_eq!(
            world
                .svc
                .remove_agent_contact(caller.user, target.user, contact.user)
                .await,
            Err(DomainError::AgentNotFound)
        );
        assert_eq!(
            world
                .svc
                .resolve_request(caller.user, target.user, contact.user, OwnerAction::Block)
                .await,
            Err(DomainError::AgentNotFound)
        );
    }
    assert_eq!(
        world.svc.agent_policy(owner.user, UserId::new()).await,
        Err(DomainError::AgentNotFound)
    );
    world.svc.user_deleted(agent.user).await.unwrap();
    assert_eq!(
        world.svc.agent_policy(owner.user, agent.user).await,
        Err(DomainError::AgentNotFound),
        "a deleted agent is gone for its owner too"
    );
}

pub(super) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        ask_holds_requests_from_people_and_agents_for_the_owner,
        the_owner_approves_declines_or_blocks_waiting_requests,
        agents_cannot_decide_on_their_requests,
        auto_own_accepts_the_owners_agents_only,
        auto_owner_contacts_accepts_the_owners_circle,
        auto_any_accepts_everyone,
        refuse_all_looks_like_a_decline_to_the_requester,
        a_refusal_ends_early_when_the_agent_asks_back,
        waiting_requests_survive_a_policy_change,
        agents_requests_are_limited,
        agents_cannot_flood_other_owners,
        agents_cannot_make_contacts_through_invites,
        the_owner_lists_and_removes_contacts,
        only_the_owner_reaches_the_agent,
    ]
}
