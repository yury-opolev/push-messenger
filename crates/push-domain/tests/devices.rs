mod common;

use common::World;
use push_domain::{DeviceId, DomainError, UserId};

#[tokio::test]
async fn registering_a_device_for_an_unknown_user_fails() {
    let world = World::new();
    let result = world
        .svc
        .register_device(DeviceId::new(), UserId::new())
        .await;
    assert_eq!(result, Err(DomainError::UnknownUser));
}

#[tokio::test]
async fn identify_accepts_the_users_own_active_device() {
    let world = World::new();
    let alice = world.person().await;
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}

#[tokio::test]
async fn identify_rejects_a_device_of_another_user() {
    let world = World::new();
    let alice = world.person().await;
    let bob = world.person().await;
    assert_eq!(
        world.svc.identify(bob.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

#[tokio::test]
async fn identify_rejects_a_revoked_device() {
    let world = World::new();
    let alice = world.person().await;
    world.svc.revoke_device(alice.device).await.unwrap();
    assert_eq!(
        world.svc.identify(alice.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

#[tokio::test]
async fn a_device_id_of_another_user_cannot_be_registered() {
    let world = World::new();
    let alice = world.person().await;
    let bob = world.person().await;
    assert_eq!(
        world.svc.register_device(alice.device, bob.user).await,
        Err(DomainError::UnknownDevice)
    );
}

#[tokio::test]
async fn a_revoked_device_cannot_be_registered_again() {
    let world = World::new();
    let alice = world.person().await;
    world.svc.revoke_device(alice.device).await.unwrap();
    assert_eq!(
        world.svc.register_device(alice.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

#[tokio::test]
async fn registering_the_same_user_and_device_twice_is_harmless() {
    let world = World::new();
    let alice = world.person().await;
    world.svc.register_user(alice.user).await.unwrap();
    world
        .svc
        .register_device(alice.device, alice.user)
        .await
        .unwrap();
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}
