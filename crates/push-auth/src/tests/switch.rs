//! Switching to Entra ID only (spec AC11): password devices are signed out at once,
//! idempotently and once each under concurrent sweeps; switching back revives none.

use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Offer, World, entra_user, password_user, world};
use crate::error::AuthError;
use crate::server::password_sweeper;
use crate::service::{Auth, SignIn};

fn entra_only(world: &World) -> Auth {
    world.auth.clone().with_sign_in(SignIn {
        password: false,
        entra: world.auth.entra().cloned(),
    })
}

/// The `device.revoked` events in the outbox: their device ids.
async fn revoked_events(world: &World) -> Vec<Value> {
    world
        .outbox()
        .await
        .into_iter()
        .filter(|(kind, _)| kind == "device.revoked")
        .map(|(_, payload)| payload["device_id"].clone())
        .collect()
}

async fn is_revoked(world: &World, device: Uuid) -> bool {
    sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM auth.device WHERE device_id = $1")
        .bind(device)
        .fetch_one(&world.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn entra_only_signs_password_devices_out_and_switching_back_revives_none() {
    let world = world(Offer::Both).await;
    let alice = password_user(&world.auth, &world.pool, "alice", 2).await;
    let erin = entra_user(&world.auth, Uuid::now_v7()).await;
    let entra_only = world.auth.clone().with_sign_in(SignIn {
        password: false,
        entra: world.auth.entra().cloned(),
    });
    // Before the sweep runs, refresh already refuses (and revokes) a password device.
    assert_eq!(
        entra_only.refresh(&alice[0].refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    let never = CancellationToken::new();
    assert_eq!(entra_only.revoke_password_devices(&never).await.unwrap(), 1);
    assert_eq!(
        entra_only.revoke_password_devices(&never).await.unwrap(),
        0,
        "idempotent"
    );
    for tokens in &alice {
        assert_eq!(
            entra_only.authenticate(&tokens.access_token).await,
            Err(AuthError::Unauthorized)
        );
    }
    assert!(entra_only.refresh(&erin.refresh_token).await.is_ok());
    let relayed: Vec<Value> = world
        .outbox()
        .await
        .into_iter()
        .filter(|(kind, _)| kind == "device.revoked")
        .map(|(_, payload)| payload["device_id"].clone())
        .collect();
    assert_eq!(relayed.len(), 2);
    assert!(!relayed.contains(&serde_json::json!(erin.device_id)));

    // Password sign-in on again: the revoked devices stay revoked.
    for tokens in &alice {
        assert_eq!(
            world.auth.refresh(&tokens.refresh_token).await,
            Err(AuthError::Unauthorized)
        );
    }
    assert_eq!(
        world
            .auth
            .login_device("alice@example.com", "correct horse", alice[0].device_id)
            .await,
        Err(AuthError::DeviceNotFound)
    );
    let registration = world
        .auth
        .login("alice@example.com", "correct horse")
        .await
        .unwrap()
        .token;
    let fresh = world
        .auth
        .register_device(&registration, Uuid::now_v7(), "Phone", "ios")
        .await
        .unwrap();
    assert!(
        alice
            .iter()
            .all(|tokens| tokens.device_id != fresh.device_id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sweeps_revoke_each_device_once() {
    let world = world(Offer::Both).await;
    for n in 0..6 {
        password_user(&world.auth, &world.pool, &format!("user{n}"), 3).await;
    }
    let entra_only = world.auth.clone().with_sign_in(SignIn {
        password: false,
        entra: world.auth.entra().cloned(),
    });
    let sweeps: Vec<_> = (0..4)
        .map(|_| {
            let auth = entra_only.clone();
            tokio::spawn(async move {
                auth.revoke_password_devices(&CancellationToken::new())
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut total = 0;
    for sweep in sweeps {
        total += sweep.await.unwrap();
    }
    assert_eq!(total, 18);
    let events: i64 =
        sqlx::query_scalar("SELECT count(*) FROM auth.outbox WHERE kind = 'device.revoked'")
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert_eq!(events, 18);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_locked_during_the_sweep_is_still_revoked_once_released() {
    let world = world(Offer::Both).await;
    let alice = password_user(&world.auth, &world.pool, "alice", 3).await;
    // Another transaction (an old replica's refresh, say) holds one device row.
    let mut holder = world.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM auth.device WHERE device_id = $1 FOR UPDATE")
        .bind(alice[1].device_id)
        .execute(&mut *holder)
        .await
        .unwrap();
    let auth = entra_only(&world);
    let sweep = tokio::spawn(async move {
        auth.revoke_password_devices(&CancellationToken::new())
            .await
    });
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        !sweep.is_finished(),
        "the sweep waits for the locked device"
    );
    assert!(is_revoked(&world, alice[0].device_id).await);
    assert!(!is_revoked(&world, alice[1].device_id).await);
    holder.rollback().await.unwrap();
    let revoked = tokio::time::timeout(Duration::from_secs(10), sweep)
        .await
        .expect("the sweep ends once the row is free")
        .unwrap()
        .unwrap();
    assert_eq!(revoked, 3);
    for tokens in &alice {
        assert!(is_revoked(&world, tokens.device_id).await);
    }
    assert_eq!(revoked_events(&world).await.len(), 3);
}

#[tokio::test]
async fn a_refresh_by_a_password_device_while_entra_only_revokes_it() {
    let world = world(Offer::Both).await;
    let alice = password_user(&world.auth, &world.pool, "alice", 2).await;
    assert_eq!(
        entra_only(&world).refresh(&alice[0].refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    assert!(is_revoked(&world, alice[0].device_id).await);
    assert!(!is_revoked(&world, alice[1].device_id).await);
    assert_eq!(
        revoked_events(&world).await,
        [serde_json::json!(alice[0].device_id)]
    );
    // Its tokens are gone for good, even with password sign-in back on.
    assert_eq!(
        world.auth.refresh(&alice[0].refresh_token).await,
        Err(AuthError::Unauthorized)
    );
}

#[tokio::test]
async fn entra_only_refuses_the_access_tokens_of_password_devices() {
    let world = world(Offer::Both).await;
    let alice = password_user(&world.auth, &world.pool, "alice", 1).await;
    let erin = entra_user(&world.auth, Uuid::now_v7()).await;
    let entra_only = entra_only(&world);
    // Not revoked yet (no sweep ran), and still refused.
    assert_eq!(
        entra_only.authenticate(&alice[0].access_token).await,
        Err(AuthError::Unauthorized)
    );
    assert!(
        world
            .auth
            .authenticate(&alice[0].access_token)
            .await
            .is_ok()
    );
    assert!(entra_only.authenticate(&erin.access_token).await.is_ok());
}

#[tokio::test]
async fn the_periodic_sweep_revokes_password_devices_created_after_the_first() {
    let world = world(Offer::Both).await;
    let entra_only = entra_only(&world);
    let shutdown = CancellationToken::new();
    assert_eq!(
        entra_only.revoke_password_devices(&shutdown).await.unwrap(),
        0
    );
    let sweeper = password_sweeper(entra_only, Duration::from_millis(100), shutdown.clone());
    // An old replica, password sign-in still on, registers a device during the rollout.
    let late = password_user(&world.auth, &world.pool, "late", 1).await;
    let mut revoked = false;
    for _ in 0..100 {
        if is_revoked(&world, late[0].device_id).await {
            revoked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(revoked, "the periodic sweep revoked the late device");
    assert_eq!(
        revoked_events(&world).await,
        [serde_json::json!(late[0].device_id)]
    );
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), sweeper)
        .await
        .expect("the sweeper stops at shutdown")
        .unwrap();
}
