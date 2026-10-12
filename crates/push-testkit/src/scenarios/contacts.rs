use crate::{Backend, Case, Person, World, case_list};
use push_domain::{ContactState, ContactSummary, DomainError, Event, UserId, ValidationError};

fn changed(user: &Person, peer: &Person, state: Option<ContactState>) -> Event {
    Event::ContactChanged {
        user: user.user,
        peer: peer.user,
        state,
    }
}

async fn request_then_accept_makes_both_sides_accepted<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);

    assert_eq!(
        world.svc.contact_request(alice.user, bob.user).await,
        Ok(ContactState::RequestedOut)
    );
    assert_eq!(
        world.svc.contact_state(bob.user, alice.user).await,
        Ok(Some(ContactState::RequestedIn))
    );

    world
        .svc
        .contact_accept(bob.user, alice.user)
        .await
        .unwrap();
    assert_eq!(
        world.svc.contact_state(alice.user, bob.user).await,
        Ok(Some(ContactState::Accepted))
    );
    assert_eq!(
        world.svc.contact_state(bob.user, alice.user).await,
        Ok(Some(ContactState::Accepted))
    );
}

async fn crossing_requests_become_accepted<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    assert_eq!(
        world.svc.contact_request(bob.user, alice.user).await,
        Ok(ContactState::Accepted)
    );
    assert_eq!(
        world.svc.contact_state(alice.user, bob.user).await,
        Ok(Some(ContactState::Accepted))
    );
}

async fn only_the_recipient_can_accept_a_request<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    assert_eq!(
        world.svc.contact_accept(alice.user, bob.user).await,
        Err(DomainError::NoPendingRequest)
    );
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    assert_eq!(
        world.svc.contact_accept(alice.user, bob.user).await,
        Err(DomainError::NoPendingRequest)
    );
}

async fn declining_removes_the_request_on_both_sides<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    world
        .svc
        .contact_decline(bob.user, alice.user)
        .await
        .unwrap();
    assert_eq!(
        world.svc.contact_state(alice.user, bob.user).await,
        Ok(None)
    );
    assert_eq!(
        world.svc.contact_state(bob.user, alice.user).await,
        Ok(None)
    );
}

async fn a_block_is_invisible_to_the_blocked_user<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world.svc.contact_block(alice.user, bob.user).await.unwrap();

    assert_eq!(
        world.svc.contact_request(bob.user, alice.user).await,
        Ok(ContactState::RequestedOut)
    );
    assert_eq!(
        world.svc.contact_state(alice.user, bob.user).await,
        Ok(Some(ContactState::Blocked))
    );
}

async fn the_blocker_cannot_send_requests<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert_eq!(
        world.svc.contact_request(alice.user, bob.user).await,
        Err(DomainError::Blocked)
    );
}

async fn blocking_ends_an_accepted_contact<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert_eq!(
        world.svc.contact_state(bob.user, alice.user).await,
        Ok(None)
    );
}

async fn unblocking_allows_a_fresh_request<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    world
        .svc
        .contact_unblock(alice.user, bob.user)
        .await
        .unwrap();
    assert_eq!(
        world.svc.contact_state(alice.user, bob.user).await,
        Ok(None)
    );
    assert_eq!(
        world.svc.contact_request(alice.user, bob.user).await,
        Ok(ContactState::RequestedOut)
    );
}

async fn a_user_cannot_request_themselves<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    assert_eq!(
        world.svc.contact_request(alice.user, alice.user).await,
        Err(DomainError::Invalid(ValidationError::SelfTarget))
    );
}

async fn requesting_an_unknown_user_fails<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    assert_eq!(
        world.svc.contact_request(alice.user, UserId::new()).await,
        Err(DomainError::UnknownUser)
    );
}

async fn a_hidden_request_arrives_when_the_block_is_lifted<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    world
        .svc
        .contact_request(bob.user, alice.user)
        .await
        .unwrap();
    world
        .svc
        .contact_unblock(alice.user, bob.user)
        .await
        .unwrap();

    assert_eq!(
        world.svc.contact_state(alice.user, bob.user).await,
        Ok(Some(ContactState::RequestedIn))
    );
    assert_eq!(
        world.svc.contact_state(bob.user, alice.user).await,
        Ok(Some(ContactState::RequestedOut))
    );
    world
        .svc
        .contact_accept(alice.user, bob.user)
        .await
        .unwrap();
    for (me, peer) in [(alice.user, bob.user), (bob.user, alice.user)] {
        assert_eq!(
            world.svc.contact_state(me, peer).await,
            Ok(Some(ContactState::Accepted))
        );
    }
}

async fn the_sender_cannot_decline_their_own_request<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    assert_eq!(
        world.svc.contact_decline(alice.user, bob.user).await,
        Err(DomainError::NoPendingRequest)
    );
}

async fn blocking_cancels_a_pending_request_in_either_direction<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    world.svc.contact_block(bob.user, alice.user).await.unwrap();
    assert_eq!(
        world.svc.contact_state(alice.user, bob.user).await,
        Ok(None)
    );

    let (carol, dave) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(carol.user, dave.user)
        .await
        .unwrap();
    world
        .svc
        .contact_block(carol.user, dave.user)
        .await
        .unwrap();
    assert_eq!(
        world.svc.contact_state(dave.user, carol.user).await,
        Ok(None)
    );
}

async fn a_repeated_request_changes_nothing<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    for _ in 0..2 {
        assert_eq!(
            world.svc.contact_request(alice.user, bob.user).await,
            Ok(ContactState::RequestedOut)
        );
    }
    assert_eq!(
        world.svc.contact_state(bob.user, alice.user).await,
        Ok(Some(ContactState::RequestedIn))
    );
}

async fn a_request_to_an_accepted_contact_stays_accepted<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;
    assert_eq!(
        world.svc.contact_request(alice.user, bob.user).await,
        Ok(ContactState::Accepted)
    );
    assert_eq!(
        world.svc.contact_state(bob.user, alice.user).await,
        Ok(Some(ContactState::Accepted))
    );
}

async fn contact_operations_reject_self_and_unknown_users<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let selfish = Err(DomainError::Invalid(ValidationError::SelfTarget));
    assert_eq!(
        world.svc.contact_accept(alice.user, alice.user).await,
        selfish
    );
    assert_eq!(
        world.svc.contact_decline(alice.user, alice.user).await,
        selfish
    );
    assert_eq!(
        world.svc.contact_block(alice.user, alice.user).await,
        selfish
    );
    let stranger = UserId::new();
    assert_eq!(
        world.svc.contact_accept(alice.user, stranger).await,
        Err(DomainError::UnknownUser)
    );
    assert_eq!(
        world.svc.contact_decline(alice.user, stranger).await,
        Err(DomainError::UnknownUser)
    );
    assert_eq!(
        world.svc.contact_block(alice.user, stranger).await,
        Err(DomainError::UnknownUser)
    );
}

async fn a_request_and_its_acceptance_are_announced_to_both_users<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);

    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [
            changed(&alice, &bob, Some(ContactState::RequestedOut)),
            changed(&bob, &alice, Some(ContactState::RequestedIn)),
        ]
    );

    world
        .svc
        .contact_accept(bob.user, alice.user)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [
            changed(&bob, &alice, Some(ContactState::Accepted)),
            changed(&alice, &bob, Some(ContactState::Accepted)),
        ]
    );
}

async fn crossing_requests_announce_acceptance_to_both_users<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    world.notifier.take();

    world
        .svc
        .contact_request(bob.user, alice.user)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [
            changed(&bob, &alice, Some(ContactState::Accepted)),
            changed(&alice, &bob, Some(ContactState::Accepted)),
        ]
    );
}

async fn a_request_that_changes_nothing_publishes_nothing<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    world.notifier.take();

    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    assert!(world.notifier.take().is_empty());
}

async fn a_decline_is_announced_only_to_the_user_who_declined<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    world.notifier.take();

    world
        .svc
        .contact_decline(bob.user, alice.user)
        .await
        .unwrap();
    assert_eq!(world.notifier.take(), [changed(&bob, &alice, None)]);
}

async fn a_block_is_announced_only_to_the_blocker<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;
    world.notifier.take();

    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [changed(&alice, &bob, Some(ContactState::Blocked))]
    );
}

async fn a_request_hidden_by_a_block_is_announced_only_when_the_block_is_lifted<B: Backend>(
    backend: &B,
) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    world.notifier.take();

    world
        .svc
        .contact_request(bob.user, alice.user)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [changed(&bob, &alice, Some(ContactState::RequestedOut))]
    );

    world
        .svc
        .contact_unblock(alice.user, bob.user)
        .await
        .unwrap();
    assert_eq!(
        world.notifier.take(),
        [changed(&alice, &bob, Some(ContactState::RequestedIn))]
    );
}

async fn unblocking_someone_who_is_not_blocked_publishes_nothing<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_unblock(alice.user, bob.user)
        .await
        .unwrap();
    assert!(world.notifier.take().is_empty());
}

async fn contacts_are_listed_from_the_owners_side_only<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
        world.person().await,
    );
    world.befriend(&alice, &bob).await;
    world
        .svc
        .contact_request(alice.user, carol.user)
        .await
        .unwrap();
    world
        .svc
        .contact_block(alice.user, dave.user)
        .await
        .unwrap();

    let contact = |person: &Person, state| ContactSummary {
        user_id: person.user,
        state,
    };
    let mut expected = vec![
        contact(&bob, ContactState::Accepted),
        contact(&carol, ContactState::RequestedOut),
        contact(&dave, ContactState::Blocked),
    ];
    expected.sort_by_key(|summary| summary.user_id);
    assert_eq!(world.svc.list_contacts(alice.user).await, Ok(expected));
    assert_eq!(world.svc.list_contacts(dave.user).await, Ok(vec![]));
    assert_eq!(
        world.svc.list_contacts(carol.user).await,
        Ok(vec![contact(&alice, ContactState::RequestedIn)])
    );
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        request_then_accept_makes_both_sides_accepted,
        crossing_requests_become_accepted,
        only_the_recipient_can_accept_a_request,
        declining_removes_the_request_on_both_sides,
        a_block_is_invisible_to_the_blocked_user,
        the_blocker_cannot_send_requests,
        blocking_ends_an_accepted_contact,
        unblocking_allows_a_fresh_request,
        a_user_cannot_request_themselves,
        requesting_an_unknown_user_fails,
        a_hidden_request_arrives_when_the_block_is_lifted,
        the_sender_cannot_decline_their_own_request,
        blocking_cancels_a_pending_request_in_either_direction,
        a_repeated_request_changes_nothing,
        a_request_to_an_accepted_contact_stays_accepted,
        contact_operations_reject_self_and_unknown_users,
        a_request_and_its_acceptance_are_announced_to_both_users,
        crossing_requests_announce_acceptance_to_both_users,
        a_request_that_changes_nothing_publishes_nothing,
        a_decline_is_announced_only_to_the_user_who_declined,
        a_block_is_announced_only_to_the_blocker,
        a_request_hidden_by_a_block_is_announced_only_when_the_block_is_lifted,
        unblocking_someone_who_is_not_blocked_publishes_nothing,
        contacts_are_listed_from_the_owners_side_only,
    ]
}
