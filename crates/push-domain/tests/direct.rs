mod common;

use common::{World, bodies};
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

#[tokio::test]
async fn every_device_of_both_users_starts_at_the_beginning() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let alice_phone = world.add_device(&alice).await;
    let bob_old = world.add_device(&bob).await;
    world.svc.revoke_device(bob_old).await.unwrap();
    let chat = world.direct_chat(&alice, &bob).await;

    for device in [alice.device, alice_phone, bob.device] {
        assert_eq!(world.position(device, chat).await, Some(0));
    }
    assert_eq!(world.position(bob_old, chat).await, None);
    world.send(&alice, chat, "hello").await;

    for device in [alice_phone, bob.device] {
        let batch = world.svc.sync(device, 100).await.unwrap();
        assert_eq!(bodies(&batch.entries), ["hello"]);
    }
    assert_eq!(
        world.svc.sync(bob_old, 100).await,
        Err(DomainError::UnknownDevice)
    );
}

#[tokio::test]
async fn opening_again_adds_no_duplicate_members() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    let chat = world.direct_chat(&alice, &bob).await;
    let again = world.svc.open_direct(alice.user, bob.user).await.unwrap();
    let from_bob = world.svc.open_direct(bob.user, alice.user).await.unwrap();
    assert_eq!(again, chat);
    assert_eq!(from_bob, chat);

    let state = world
        .svc
        .conversation_state(alice.user, chat)
        .await
        .unwrap();
    assert_eq!(state.members.len(), 2);
}

#[tokio::test]
async fn opening_after_a_block_is_refused_even_if_the_chat_exists() {
    let world = World::new();
    let (alice, bob) = (world.person().await, world.person().await);
    world.direct_chat(&alice, &bob).await;
    world.svc.contact_block(bob.user, alice.user).await.unwrap();

    assert_eq!(
        world.svc.open_direct(alice.user, bob.user).await,
        Err(DomainError::NotContact)
    );
}
