mod common;

use common::{Person, World};
use push_domain::{ContactState, DomainError, Event, UserId, ValidationError};

fn changed(user: &Person, peer: &Person, state: Option<ContactState>) -> Event {
    Event::ContactChanged {
        user: user.user,
        peer: peer.user,
        state,
    }
}

#[tokio::test]
async fn request_then_accept_makes_both_sides_accepted() {
    let world = World::new();
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

#[tokio::test]
async fn crossing_requests_become_accepted() {
    let world = World::new();
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

#[tokio::test]
async fn only_the_recipient_can_accept_a_request() {
    let world = World::new();
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

#[tokio::test]
async fn declining_removes_the_request_on_both_sides() {
    let world = World::new();
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

#[tokio::test]
async fn a_block_is_invisible_to_the_blocked_user() {
    let world = World::new();
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

#[tokio::test]
async fn the_blocker_cannot_send_requests() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert_eq!(
        world.svc.contact_request(alice.user, bob.user).await,
        Err(DomainError::Blocked)
    );
}

#[tokio::test]
async fn blocking_ends_an_accepted_contact() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;
    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert_eq!(
        world.svc.contact_state(bob.user, alice.user).await,
        Ok(None)
    );
}

#[tokio::test]
async fn unblocking_allows_a_fresh_request() {
    let world = World::new();
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

#[tokio::test]
async fn a_user_cannot_request_themselves() {
    let world = World::new();
    let alice = world.person().await;
    assert_eq!(
        world.svc.contact_request(alice.user, alice.user).await,
        Err(DomainError::Invalid(ValidationError::SelfTarget))
    );
}

#[tokio::test]
async fn requesting_an_unknown_user_fails() {
    let world = World::new();
    let alice = world.person().await;
    assert_eq!(
        world.svc.contact_request(alice.user, UserId::new()).await,
        Err(DomainError::UnknownUser)
    );
}

#[tokio::test]
async fn a_hidden_request_arrives_when_the_block_is_lifted() {
    let world = World::new();
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

#[tokio::test]
async fn the_sender_cannot_decline_their_own_request() {
    let world = World::new();
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

#[tokio::test]
async fn blocking_cancels_a_pending_request_in_either_direction() {
    let world = World::new();
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

#[tokio::test]
async fn a_repeated_request_changes_nothing() {
    let world = World::new();
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

#[tokio::test]
async fn a_request_to_an_accepted_contact_stays_accepted() {
    let world = World::new();
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

#[tokio::test]
async fn contact_operations_reject_self_and_unknown_users() {
    let world = World::new();
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

#[tokio::test]
async fn a_request_and_its_acceptance_are_announced_to_both_users() {
    let world = World::new();
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

#[tokio::test]
async fn crossing_requests_announce_acceptance_to_both_users() {
    let world = World::new();
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

#[tokio::test]
async fn a_request_that_changes_nothing_publishes_nothing() {
    let world = World::new();
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

#[tokio::test]
async fn a_decline_is_announced_only_to_the_user_who_declined() {
    let world = World::new();
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

#[tokio::test]
async fn a_block_is_announced_only_to_the_blocker() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;
    world.notifier.take();

    world.svc.contact_block(alice.user, bob.user).await.unwrap();
    assert_eq!(
        world.notifier.take(),
        [changed(&alice, &bob, Some(ContactState::Blocked))]
    );
}

#[tokio::test]
async fn a_request_hidden_by_a_block_is_announced_only_when_the_block_is_lifted() {
    let world = World::new();
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

#[tokio::test]
async fn unblocking_someone_who_is_not_blocked_publishes_nothing() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_unblock(alice.user, bob.user)
        .await
        .unwrap();
    assert!(world.notifier.take().is_empty());
}
