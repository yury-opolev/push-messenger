mod common;

use common::World;
use push_domain::{ContactState, DomainError, UserId, ValidationError};

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
