use push_domain::{DeviceId, DomainError, Store, Tx, UserId};

use super::add_users;
use crate::{Backend, Case, case_list};

async fn inserting_a_user_twice_is_harmless<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    let mut tx = store.begin().await.unwrap();
    tx.insert_user(user).await.unwrap();
    tx.insert_user(user).await.unwrap();

    assert!(tx.user_exists(user).await.unwrap());
    assert!(!tx.user_exists(UserId::new()).await.unwrap());
}

async fn revoked_devices_have_no_owner_and_are_not_listed<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (user, other) = (UserId::new(), UserId::new());
    add_users(&store, &[user, other]).await;
    let (kept, revoked, others) = (DeviceId::new(), DeviceId::new(), DeviceId::new());
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(kept, user).await.unwrap();
    tx.insert_device(revoked, user).await.unwrap();
    tx.insert_device(others, other).await.unwrap();
    tx.revoke_device(revoked).await.unwrap();

    assert_eq!(tx.device_owner(kept).await.unwrap(), Some(user));
    assert_eq!(tx.device_owner(revoked).await.unwrap(), None);
    assert_eq!(tx.device_owner(DeviceId::new()).await.unwrap(), None);
    assert_eq!(tx.devices_of_user(user).await.unwrap(), vec![kept]);
}

async fn inserting_a_known_device_id_changes_nothing<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (user, other) = (UserId::new(), UserId::new());
    add_users(&store, &[user, other]).await;
    let device = DeviceId::new();
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(device, user).await.unwrap();
    tx.insert_device(device, other).await.unwrap();

    assert_eq!(tx.device_owner(device).await.unwrap(), Some(user));
    assert!(tx.devices_of_user(other).await.unwrap().is_empty());
    assert!(tx.device_exists(device).await.unwrap());
    tx.revoke_device(device).await.unwrap();
    assert!(tx.device_exists(device).await.unwrap());
    assert!(!tx.device_exists(DeviceId::new()).await.unwrap());
}

async fn a_device_of_an_unknown_user_is_refused<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_device(DeviceId::new(), UserId::new()).await,
        Err(DomainError::Store(_))
    ));
}

async fn revoking_an_unknown_device_does_nothing<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.revoke_device(DeviceId::new()).await, Ok(()));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        inserting_a_user_twice_is_harmless,
        revoked_devices_have_no_owner_and_are_not_listed,
        inserting_a_known_device_id_changes_nothing,
        a_device_of_an_unknown_user_is_refused,
        revoking_an_unknown_device_does_nothing,
    ]
}
