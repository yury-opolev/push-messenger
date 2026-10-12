use crate::{Backend, Case, World, case_list};
use push_domain::{DeviceId, DomainError, UserId};

async fn registering_a_device_for_an_unknown_user_fails<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let result = world
        .svc
        .register_device(DeviceId::new(), UserId::new())
        .await;
    assert_eq!(result, Err(DomainError::UnknownUser));
}

async fn identify_accepts_the_users_own_active_device<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}

async fn identify_rejects_a_device_of_another_user<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let bob = world.person().await;
    assert_eq!(
        world.svc.identify(bob.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn identify_rejects_a_revoked_device<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    world.svc.revoke_device(alice.device).await.unwrap();
    assert_eq!(
        world.svc.identify(alice.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn a_device_id_of_another_user_cannot_be_registered<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let bob = world.person().await;
    assert_eq!(
        world.svc.register_device(alice.device, bob.user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn a_revoked_device_cannot_be_registered_again<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    world.svc.revoke_device(alice.device).await.unwrap();
    assert_eq!(
        world.svc.register_device(alice.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn registering_the_same_user_and_device_twice_is_harmless<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    world.svc.register_user(alice.user).await.unwrap();
    world
        .svc
        .register_device(alice.device, alice.user)
        .await
        .unwrap();
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        registering_a_device_for_an_unknown_user_fails,
        identify_accepts_the_users_own_active_device,
        identify_rejects_a_device_of_another_user,
        identify_rejects_a_revoked_device,
        a_device_id_of_another_user_cannot_be_registered,
        a_revoked_device_cannot_be_registered_again,
        registering_the_same_user_and_device_twice_is_harmless,
    ]
}
