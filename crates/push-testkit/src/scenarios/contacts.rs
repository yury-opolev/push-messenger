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

async fn profiles_are_visible_to_relations_and_conversation_members<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let (dave, erin, stranger) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    // Carol and Dave share Alice's group without being contacts.
    world.group(&alice, &[&carol, &dave]).await;
    world
        .svc
        .contact_request(erin.user, alice.user)
        .await
        .unwrap();
    world
        .svc
        .contact_block(alice.user, erin.user)
        .await
        .unwrap();

    let visible = |user: &Person, candidates: &[&Person]| {
        let candidates: Vec<UserId> = candidates.iter().map(|person| person.user).collect();
        let svc = &world.svc;
        let user = user.user;
        async move { svc.visible_users(user, &candidates).await.unwrap() }
    };
    let sorted = |people: &[&Person]| {
        let mut users: Vec<UserId> = people.iter().map(|person| person.user).collect();
        users.sort();
        users
    };
    assert_eq!(
        visible(&bob, &[&alice]).await,
        [alice.user],
        "a request Bob received"
    );
    assert!(
        visible(&alice, &[&bob]).await.is_empty(),
        "Alice's own pending request grants nothing"
    );
    assert_eq!(
        visible(&carol, &[&dave, &carol, &stranger, &dave]).await,
        sorted(&[&carol, &dave])
    );
    assert_eq!(
        visible(&alice, &[&erin]).await,
        [erin.user],
        "Alice blocked Erin"
    );
    assert!(
        visible(&erin, &[&alice]).await.is_empty(),
        "the block stays hidden"
    );
    assert!(visible(&stranger, &[&alice, &bob]).await.is_empty());
}

async fn a_blocked_user_who_sends_a_new_request_still_cannot_see_the_blocker<B: Backend>(
    backend: &B,
) {
    let world = World::new(backend).await;
    let (alice, xavier) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(xavier.user, alice.user)
        .await
        .unwrap();
    world
        .svc
        .contact_block(alice.user, xavier.user)
        .await
        .unwrap();
    world
        .svc
        .contact_request(xavier.user, alice.user)
        .await
        .unwrap();
    assert_eq!(
        world.svc.contact_state(xavier.user, alice.user).await,
        Ok(Some(ContactState::RequestedOut))
    );
    let seen_by_xavier = world.svc.visible_users(xavier.user, &[alice.user]).await;
    assert_eq!(seen_by_xavier, Ok(vec![]), "the new request grants nothing");
    let seen_by_alice = world.svc.visible_users(alice.user, &[xavier.user]).await;
    assert_eq!(seen_by_alice, Ok(vec![xavier.user]), "Alice blocked Xavier");
}

async fn blocking_someone_gives_no_sight_of_a_peer_the_blocker_never_saw<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, xavier, stranger) = (
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let visible = |user: &Person, other: &Person| {
        let (svc, user, other) = (&world.svc, user.user, other.user);
        async move { svc.visible_users(user, &[other]).await.unwrap() }
    };
    world
        .svc
        .contact_block(alice.user, xavier.user)
        .await
        .unwrap();
    world
        .svc
        .contact_block(xavier.user, alice.user)
        .await
        .unwrap();
    assert!(
        visible(&xavier, &alice).await.is_empty(),
        "Xavier's own block on a stranger grants nothing"
    );
    assert!(
        visible(&alice, &xavier).await.is_empty(),
        "Alice never saw Xavier before blocking him"
    );
    world
        .svc
        .contact_block(stranger.user, alice.user)
        .await
        .unwrap();
    assert!(visible(&stranger, &alice).await.is_empty());
    // Asking first and then blocking is no better: the pending request granted nothing.
    world
        .svc
        .contact_request(alice.user, stranger.user)
        .await
        .unwrap();
    world
        .svc
        .contact_block(alice.user, stranger.user)
        .await
        .unwrap();
    assert!(visible(&alice, &stranger).await.is_empty());
}

async fn a_block_keeps_the_sight_the_blocker_had<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob, carol, dave) = (
        world.person().await,
        world.person().await,
        world.person().await,
        world.person().await,
    );
    let visible = |user: &Person, other: &Person| {
        let (svc, user, other) = (&world.svc, user.user, other.user);
        async move { svc.visible_users(user, &[other]).await.unwrap() }
    };
    // Accepted, then blocked.
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    world
        .svc
        .contact_accept(bob.user, alice.user)
        .await
        .unwrap();
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert_eq!(visible(&alice, &bob).await, [bob.user]);
    assert!(visible(&bob, &alice).await.is_empty());
    // Blocking again changes nothing.
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert_eq!(visible(&alice, &bob).await, [bob.user]);
    // Carol's block back grants nothing: the row of her request vanished when Alice blocked.
    world
        .svc
        .contact_request(carol.user, alice.user)
        .await
        .unwrap();
    world
        .svc
        .contact_block(alice.user, carol.user)
        .await
        .unwrap();
    world
        .svc
        .contact_block(carol.user, alice.user)
        .await
        .unwrap();
    assert!(visible(&carol, &alice).await.is_empty());
    assert_eq!(visible(&alice, &carol).await, [carol.user]);
    // A shared conversation still shows names, block or no block.
    world.group(&alice, &[&dave]).await;
    world
        .svc
        .contact_block(dave.user, alice.user)
        .await
        .unwrap();
    assert_eq!(visible(&dave, &alice).await, [alice.user]);
    // Unblocking forgets the sight: a fresh block of a stranger grants nothing.
    world
        .svc
        .contact_unblock(alice.user, bob.user)
        .await
        .unwrap();
    assert!(visible(&alice, &bob).await.is_empty());
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert!(visible(&alice, &bob).await.is_empty());
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
        profiles_are_visible_to_relations_and_conversation_members,
        a_blocked_user_who_sends_a_new_request_still_cannot_see_the_blocker,
        blocking_someone_gives_no_sight_of_a_peer_the_blocker_never_saw,
        a_block_keeps_the_sight_the_blocker_had,
    ]
}
