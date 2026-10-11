mod common;

use common::World;
use push_domain::{DomainError, ValidationError};

#[tokio::test]
async fn a_direct_chat_requires_accepted_contacts() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world
        .svc
        .contact_request(alice.user, bob.user)
        .await
        .unwrap();
    assert_eq!(
        world.svc.open_direct(alice.user, bob.user).await,
        Err(DomainError::NotContact)
    );
}

#[tokio::test]
async fn both_users_get_the_same_direct_conversation() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;

    let from_alice = world.svc.open_direct(alice.user, bob.user).await.unwrap();
    let from_bob = world.svc.open_direct(bob.user, alice.user).await.unwrap();
    assert_eq!(from_alice, from_bob);
}

#[tokio::test]
async fn a_direct_chat_with_yourself_is_invalid() {
    let world = World::new();
    let alice = world.person().await;
    assert_eq!(
        world.svc.open_direct(alice.user, alice.user).await,
        Err(DomainError::Invalid(ValidationError::SelfTarget))
    );
}
