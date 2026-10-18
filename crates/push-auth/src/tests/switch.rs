//! Switching to Entra ID only (spec AC11): password devices are signed out at once,
//! idempotently and once each under concurrent sweeps; switching back revives none.

use serde_json::Value;
use uuid::Uuid;

use super::{Offer, entra_user, password_user, world};
use crate::error::AuthError;
use crate::service::SignIn;

#[tokio::test]
async fn entra_only_signs_password_devices_out_and_switching_back_revives_none() {
    let world = world(Offer::Both).await;
    let alice = password_user(&world.auth, &world.pool, "alice", 2).await;
    let erin = entra_user(&world.auth, Uuid::now_v7()).await;
    let entra_only = world.auth.clone().with_sign_in(SignIn {
        password: false,
        entra: world.auth.entra().cloned(),
    });
    // Before the sweep runs, refresh already refuses a password account's device.
    assert_eq!(
        entra_only.refresh(&alice[0].refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    assert_eq!(entra_only.revoke_password_devices().await.unwrap(), 2);
    assert_eq!(
        entra_only.revoke_password_devices().await.unwrap(),
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
            tokio::spawn(async move { auth.revoke_password_devices().await.unwrap() })
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
