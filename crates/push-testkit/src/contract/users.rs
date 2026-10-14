use chrono::{TimeZone, Utc};
use push_domain::{DeviceId, DeviceState, DomainError, Store, Tx, UserId};

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
    let (device, revoked) = (DeviceId::new(), DeviceId::new());
    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.insert_device(device, user).await, Ok(true));
    assert_eq!(tx.insert_device(device, user).await, Ok(true));
    assert_eq!(tx.insert_device(device, other).await, Ok(false));
    assert_eq!(tx.insert_device(revoked, user).await, Ok(true));
    tx.revoke_device(revoked).await.unwrap();
    assert_eq!(tx.insert_device(revoked, user).await, Ok(false));

    assert_eq!(tx.device_owner(device).await.unwrap(), Some(user));
    assert_eq!(tx.device_owner(revoked).await.unwrap(), None);
    assert!(tx.devices_of_user(other).await.unwrap().is_empty());
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

async fn device_user_names_the_owner_of_revoked_devices_too<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let (active, revoked) = (DeviceId::new(), DeviceId::new());
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(active, user).await.unwrap();
    tx.insert_device(revoked, user).await.unwrap();
    tx.revoke_device(revoked).await.unwrap();

    assert_eq!(tx.device_user(active).await.unwrap(), Some(user));
    assert_eq!(tx.device_user(revoked).await.unwrap(), Some(user));
    assert_eq!(tx.device_user(DeviceId::new()).await.unwrap(), None);
}

async fn device_state_reads_owner_activity_and_reset_together<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (user, other) = (UserId::new(), UserId::new());
    add_users(&store, &[user, other]).await;
    let (active, revoked) = (DeviceId::new(), DeviceId::new());
    let reset = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(active, user).await.unwrap();
    tx.insert_device(revoked, other).await.unwrap();
    tx.revoke_device(revoked).await.unwrap();
    tx.record_password_reset(user, reset).await.unwrap();

    let state = |user, active, password_reset_at| {
        Some(DeviceState {
            user,
            active,
            password_reset_at,
        })
    };
    assert_eq!(
        tx.device_state(active).await.unwrap(),
        state(user, true, Some(reset))
    );
    assert_eq!(
        tx.device_state(revoked).await.unwrap(),
        state(other, false, None)
    );
    assert_eq!(tx.device_state(DeviceId::new()).await.unwrap(), None);
}

async fn a_password_reset_keeps_the_latest_time_and_needs_the_user<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let device = DeviceId::new();
    let (earlier, later) = (
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2026, 1, 2, 12, 0, 0).unwrap(),
    );
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(device, user).await.unwrap();
    // The time kept comes back.
    assert_eq!(tx.record_password_reset(user, later).await.unwrap(), later);
    assert_eq!(
        tx.record_password_reset(user, earlier).await.unwrap(),
        later
    );
    let reset = tx
        .device_state(device)
        .await
        .unwrap()
        .unwrap()
        .password_reset_at;
    assert_eq!(reset, Some(later));
    assert!(matches!(
        tx.record_password_reset(UserId::new(), later).await,
        Err(DomainError::Store(_))
    ));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        inserting_a_user_twice_is_harmless,
        revoked_devices_have_no_owner_and_are_not_listed,
        inserting_a_known_device_id_changes_nothing,
        a_device_of_an_unknown_user_is_refused,
        revoking_an_unknown_device_does_nothing,
        device_user_names_the_owner_of_revoked_devices_too,
        device_state_reads_owner_activity_and_reset_together,
        a_password_reset_keeps_the_latest_time_and_needs_the_user,
    ]
}
