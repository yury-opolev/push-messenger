use crate::{Backend, Case, World, bodies, case_list};
use push_domain::{DomainError, Positions, ValidationError};

async fn a_direct_chat_requires_accepted_contacts<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn both_users_get_the_same_direct_conversation<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world.befriend(&alice, &bob).await;

    let from_alice = world.svc.open_direct(alice.user, bob.user).await.unwrap();
    let from_bob = world.svc.open_direct(bob.user, alice.user).await.unwrap();
    assert_eq!(from_alice, from_bob);
}

async fn a_direct_chat_with_yourself_is_invalid<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    assert_eq!(
        world.svc.open_direct(alice.user, alice.user).await,
        Err(DomainError::Invalid(ValidationError::SelfTarget))
    );
}

async fn every_device_of_both_users_starts_at_the_beginning<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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

async fn opening_again_adds_no_duplicate_members<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
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
    assert!(matches!(state.positions, Positions::Members(members) if members.len() == 2));
}

async fn opening_after_a_block_is_refused_even_if_the_chat_exists<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let (alice, bob) = (world.person().await, world.person().await);
    world.direct_chat(&alice, &bob).await;
    world.svc.contact_block(bob.user, alice.user).await.unwrap();

    assert_eq!(
        world.svc.open_direct(alice.user, bob.user).await,
        Err(DomainError::NotContact)
    );
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        a_direct_chat_requires_accepted_contacts,
        both_users_get_the_same_direct_conversation,
        a_direct_chat_with_yourself_is_invalid,
        every_device_of_both_users_starts_at_the_beginning,
        opening_again_adds_no_duplicate_members,
        opening_after_a_block_is_refused_even_if_the_chat_exists,
    ]
}
