//! Devices, tokens and profiles on a real PostgreSQL (Docker must be running). Each test
//! starts a container of its own; they run in parallel.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use push_auth::links::{LinkClaims, LinkSigner, Purpose, password_stamp};
use push_auth::password::Passwords;
use push_auth::tokens::{AccessTokens, refresh_hash};
use push_auth::{
    Auth, AuthError, Caller, MAX_ACTIVE_DEVICES, REFRESH_GRACE, Settings, Signup, migrate_database,
};
use serde_json::Value;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::sync::Barrier;
use uuid::Uuid;

const LINK_KEY: [u8; 32] = [3; 32];
const AUDIENCE: &str = "wss://api.example.com/v1/socket";

struct World {
    _container: ContainerAsync<Postgres>,
    auth: Auth,
    pool: PgPool,
}

async fn world() -> World {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    // Docker publishes ports on IPv4; "localhost" would try IPv6 first on every connect.
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    migrate_database(&url).await.expect("migrate");
    let pool = PgPool::connect(&url).await.expect("connect");
    let auth = Auth::new(
        pool.clone(),
        Settings {
            public_url: "https://api.example.com".to_owned(),
            passwords: Passwords::new(64, 1, 1).unwrap(),
            links: LinkSigner::new(LINK_KEY.to_vec()).unwrap(),
            tokens: AccessTokens::from_seed(&[5; 32], AUDIENCE).unwrap(),
            hash_concurrency: 4,
            hash_queue: 64,
            hash_wait: Duration::from_secs(5),
        },
    );
    World {
        _container: container,
        auth,
        pool,
    }
}

async fn outbox(pool: &PgPool) -> Vec<(String, Value)> {
    sqlx::query_as("SELECT kind, payload FROM auth.outbox ORDER BY created_at, event_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// A verified account `name@example.com` with the handle `name`; returns a registration
/// token.
async fn verified(world: &World, name: &str) -> String {
    let email = format!("{name}@example.com");
    world
        .auth
        .signup(Signup {
            email: email.clone(),
            password: "correct horse".to_owned(),
            handle: name.to_owned(),
            display_name: name.to_uppercase(),
        })
        .await
        .unwrap();
    let link = outbox(&world.pool)
        .await
        .into_iter()
        .rev()
        .find(|(_, payload)| payload["to"] == email.as_str())
        .map(|(_, payload)| payload["params"]["link"].as_str().unwrap().to_owned())
        .unwrap();
    let token = link.split_once("?token=").unwrap().1;
    world.auth.verify_email(token).await.unwrap();
    world
        .auth
        .login(&email, "correct horse")
        .await
        .unwrap()
        .token
}

fn caller(tokens: &push_auth::Tokens) -> Caller {
    Caller {
        user_id: tokens.user_id,
        device_id: tokens.device_id,
    }
}

#[tokio::test]
async fn a_registered_device_gets_tokens_and_is_announced() {
    let world = world().await;
    let registration = verified(&world, "alice").await;
    let tokens = world
        .auth
        .register_device(&registration, Uuid::now_v7(), "Alice's phone", "ios")
        .await
        .unwrap();
    assert_eq!(tokens.access_expires_in_s, 1800);
    assert_eq!(tokens.refresh_expires_in_s, 30 * 24 * 3600);
    assert_eq!(tokens.device_id.get_version_num(), 7);

    let claims = AccessTokens::from_seed(&[5; 32], AUDIENCE)
        .unwrap()
        .verify(&tokens.access_token)
        .unwrap();
    assert_eq!(claims.sub, tokens.user_id);
    assert_eq!(claims.device_id, tokens.device_id);
    assert!(claims.email_verified);
    assert_eq!(
        world.auth.authenticate(&tokens.access_token).await,
        Ok(caller(&tokens))
    );

    let (kind, payload) = outbox(&world.pool).await.pop().unwrap();
    assert_eq!(kind, "device.registered");
    assert_eq!(payload["user_id"], tokens.user_id.to_string());
    assert_eq!(payload["device_id"], tokens.device_id.to_string());

    // Only the hash is stored.
    let stored: Vec<Vec<u8>> = sqlx::query_scalar("SELECT token_hash FROM auth.refresh_token")
        .fetch_all(&world.pool)
        .await
        .unwrap();
    assert_eq!(stored, [refresh_hash(&tokens.refresh_token).to_vec()]);

    let devices = world.auth.list_devices(caller(&tokens)).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device_id, tokens.device_id);
    assert_eq!(devices[0].name, "Alice's phone");
    assert_eq!(devices[0].platform.as_str(), "ios");
    assert!(devices[0].last_seen_at.is_some());
}

#[tokio::test]
async fn each_token_does_only_its_own_job() {
    let world = world().await;
    let registration = verified(&world, "alice").await;
    let tokens = world
        .auth
        .register_device(&registration, Uuid::now_v7(), "Phone", "android")
        .await
        .unwrap();
    let signer = LinkSigner::new(LINK_KEY.to_vec()).unwrap();
    let expired = signer.sign(&LinkClaims {
        sub: tokens.user_id,
        purpose: Purpose::Register,
        exp: 1,
        email: None,
        pw: None,
        xid: None,
        sx: None,
    });
    let verify_link = signer.sign(&LinkClaims {
        sub: tokens.user_id,
        purpose: Purpose::Verify,
        exp: i64::MAX,
        email: Some("alice@example.com".to_owned()),
        pw: None,
        xid: None,
        sx: None,
    });
    let unstamped = signer.sign(&LinkClaims {
        sub: tokens.user_id,
        purpose: Purpose::Register,
        exp: i64::MAX,
        email: None,
        pw: None,
        xid: None,
        sx: None,
    });
    for token in [&tokens.access_token, &expired, &verify_link, &unstamped] {
        assert_eq!(
            world
                .auth
                .register_device(token, Uuid::now_v7(), "Phone", "web")
                .await,
            Err(AuthError::Unauthorized)
        );
    }
    for token in [&registration, &tokens.refresh_token] {
        assert_eq!(
            world.auth.authenticate(token).await,
            Err(AuthError::Unauthorized)
        );
    }
    let stale = AccessTokens::from_seed(&[5; 32], AUDIENCE)
        .unwrap()
        .issue(tokens.user_id, tokens.device_id, 1)
        .0;
    assert_eq!(
        world.auth.authenticate(&stale).await,
        Err(AuthError::TokenExpired)
    );
    assert!(matches!(
        world
            .auth
            .register_device(&registration, Uuid::now_v7(), "Phone", "toaster")
            .await,
        Err(AuthError::Invalid(_))
    ));
}

#[tokio::test]
async fn the_device_limit_holds_under_concurrent_registrations() {
    let world = world().await;
    let registration = Arc::new(verified(&world, "alice").await);
    let auth = Arc::new(world.auth.clone());
    let attempts = (0..MAX_ACTIVE_DEVICES + 3).map(|index| {
        let (auth, registration) = (Arc::clone(&auth), Arc::clone(&registration));
        tokio::spawn(async move {
            auth.register_device(
                &registration,
                Uuid::now_v7(),
                &format!("Device {index}"),
                "other",
            )
            .await
        })
    });
    let mut registered = Vec::new();
    let mut refused = 0;
    for attempt in attempts.collect::<Vec<_>>() {
        match attempt.await.unwrap() {
            Ok(tokens) => registered.push(tokens),
            Err(AuthError::TooManyDevices) => refused += 1,
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(registered.len() as i64, MAX_ACTIVE_DEVICES);
    assert_eq!(refused, 3);

    // Revoking one makes room for one.
    let first = &registered[0];
    world
        .auth
        .revoke_device(caller(first), first.device_id)
        .await
        .unwrap();
    world
        .auth
        .register_device(&registration, Uuid::now_v7(), "Replacement", "web")
        .await
        .unwrap();
    assert_eq!(
        world
            .auth
            .register_device(&registration, Uuid::now_v7(), "One too many", "web")
            .await,
        Err(AuthError::TooManyDevices)
    );
}

/// Moves every rotation back past the grace window, as if the time had passed.
async fn after_the_grace_window(pool: &PgPool) {
    sqlx::query(
        "UPDATE auth.refresh_token SET rotated_at = rotated_at - make_interval(secs => $1)",
    )
    .bind((REFRESH_GRACE.as_secs() + 1) as f64)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn refreshing_rotates_and_reuse_after_the_grace_window_revokes_the_device_tokens() {
    let world = world().await;
    let registration = verified(&world, "alice").await;
    let first = world
        .auth
        .register_device(&registration, Uuid::now_v7(), "Phone", "ios")
        .await
        .unwrap();
    let second = world.auth.refresh(&first.refresh_token).await.unwrap();
    assert_eq!(second.device_id, first.device_id);
    assert_ne!(second.refresh_token, first.refresh_token);
    assert_ne!(second.access_token, first.access_token);
    world.auth.authenticate(&second.access_token).await.unwrap();

    // The first token was rotated 31 s ago: using it again revokes the whole session.
    after_the_grace_window(&world.pool).await;
    assert_eq!(
        world.auth.refresh(&first.refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        world.auth.refresh(&second.refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        world.auth.refresh("never-issued").await,
        Err(AuthError::Unauthorized)
    );
    // Signing in again on the device starts a new session.
    let third = world
        .auth
        .login_device("alice@example.com", "correct horse", first.device_id)
        .await
        .unwrap();
    world.auth.refresh(&third.refresh_token).await.unwrap();
}

#[tokio::test]
async fn a_lost_refresh_answer_can_be_retried_within_the_grace_window() {
    let world = world().await;
    let registration = verified(&world, "alice").await;
    let first = world
        .auth
        .register_device(&registration, Uuid::now_v7(), "Phone", "ios")
        .await
        .unwrap();
    // The answer with `lost` never reaches the client, which retries with `first`.
    let lost = world.auth.refresh(&first.refresh_token).await.unwrap();
    let retried = world.auth.refresh(&first.refresh_token).await.unwrap();
    assert_eq!(retried.device_id, first.device_id);
    assert_ne!(retried.refresh_token, lost.refresh_token);
    world
        .auth
        .authenticate(&retried.access_token)
        .await
        .unwrap();
    // The branch that lives goes on as usual.
    let next = world.auth.refresh(&retried.refresh_token).await.unwrap();
    // The cut-off branch is reuse: the device's tokens are revoked, the live branch too.
    assert_eq!(
        world.auth.refresh(&lost.refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        world.auth.refresh(&next.refresh_token).await,
        Err(AuthError::Unauthorized)
    );
}

#[tokio::test]
async fn an_older_token_is_reuse_even_within_the_grace_window() {
    let world = world().await;
    let registration = verified(&world, "alice").await;
    let first = world
        .auth
        .register_device(&registration, Uuid::now_v7(), "Phone", "ios")
        .await
        .unwrap();
    let second = world.auth.refresh(&first.refresh_token).await.unwrap();
    let third = world.auth.refresh(&second.refresh_token).await.unwrap();
    // `first` was rotated a moment ago, but `second` is not the live token any more.
    assert_eq!(
        world.auth.refresh(&first.refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        world.auth.refresh(&third.refresh_token).await,
        Err(AuthError::Unauthorized),
        "the device's tokens are revoked"
    );
}

#[tokio::test]
async fn signing_in_on_a_device_replaces_its_session_and_needs_an_active_own_device() {
    let world = world().await;
    let alice = verified(&world, "alice").await;
    let bob = verified(&world, "bob").await;
    let phone = world
        .auth
        .register_device(&alice, Uuid::now_v7(), "Phone", "ios")
        .await
        .unwrap();
    let bobs = world
        .auth
        .register_device(&bob, Uuid::now_v7(), "Laptop", "desktop")
        .await
        .unwrap();

    let again = world
        .auth
        .login_device("alice@example.com", "correct horse", phone.device_id)
        .await
        .unwrap();
    assert_eq!(again.device_id, phone.device_id);
    assert_eq!(
        world.auth.refresh(&phone.refresh_token).await,
        Err(AuthError::Unauthorized),
        "the earlier session is revoked"
    );
    assert_eq!(
        world
            .auth
            .login_device("alice@example.com", "correct horse", bobs.device_id)
            .await,
        Err(AuthError::DeviceNotFound)
    );
    assert_eq!(
        world
            .auth
            .login_device("alice@example.com", "wrong horse", phone.device_id)
            .await,
        Err(AuthError::InvalidCredentials)
    );
    world
        .auth
        .revoke_device(caller(&again), phone.device_id)
        .await
        .unwrap();
    assert_eq!(
        world
            .auth
            .login_device("alice@example.com", "correct horse", phone.device_id)
            .await,
        Err(AuthError::DeviceNotFound)
    );
}

#[tokio::test]
async fn revoking_a_device_ends_its_tokens_and_is_announced() {
    let world = world().await;
    let alice = verified(&world, "alice").await;
    let bob = verified(&world, "bob").await;
    let phone = world
        .auth
        .register_device(&alice, Uuid::now_v7(), "Phone", "ios")
        .await
        .unwrap();
    let laptop = world
        .auth
        .register_device(&alice, Uuid::now_v7(), "Laptop", "desktop")
        .await
        .unwrap();
    let bobs = world
        .auth
        .register_device(&bob, Uuid::now_v7(), "Phone", "android")
        .await
        .unwrap();

    assert_eq!(
        world
            .auth
            .revoke_device(caller(&laptop), bobs.device_id)
            .await,
        Err(AuthError::DeviceNotFound),
        "not Alice's device"
    );
    world
        .auth
        .revoke_device(caller(&laptop), phone.device_id)
        .await
        .unwrap();
    let (kind, payload) = outbox(&world.pool).await.pop().unwrap();
    assert_eq!(kind, "device.revoked");
    assert_eq!(payload["device_id"], phone.device_id.to_string());
    assert_eq!(
        world.auth.authenticate(&phone.access_token).await,
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        world.auth.refresh(&phone.refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        world
            .auth
            .revoke_device(caller(&laptop), phone.device_id)
            .await,
        Err(AuthError::DeviceNotFound),
        "already revoked"
    );
    let listed: Vec<Uuid> = world
        .auth
        .list_devices(caller(&laptop))
        .await
        .unwrap()
        .into_iter()
        .map(|device| device.device_id)
        .collect();
    assert_eq!(listed, [laptop.device_id]);
}

#[tokio::test]
async fn a_password_reset_revokes_every_refresh_token() {
    let world = world().await;
    let alice = verified(&world, "alice").await;
    let phone = world
        .auth
        .register_device(&alice, Uuid::now_v7(), "Phone", "ios")
        .await
        .unwrap();
    world
        .auth
        .forgot_password("alice@example.com")
        .await
        .unwrap();
    let (_, payload) = outbox(&world.pool).await.pop().unwrap();
    let link = payload["params"]["link"].as_str().unwrap();
    let token = link.split_once("?token=").unwrap().1;
    world
        .auth
        .reset_password(token, "battery staple")
        .await
        .unwrap();
    assert_eq!(
        world.auth.refresh(&phone.refresh_token).await,
        Err(AuthError::Unauthorized)
    );
}

/// The cutoff message-svc applies (`iat` no later than the reset's second is refused)
/// holds at auth-svc's own API too: a stolen access token cannot keep reading the account
/// or revoking the devices its owner signs back in with.
#[tokio::test]
async fn a_password_reset_ends_earlier_access_tokens_and_a_later_sign_in_works() {
    let world = world().await;
    let alice = verified(&world, "alice").await;
    let phone = world
        .auth
        .register_device(&alice, Uuid::now_v7(), "Phone", "ios")
        .await
        .unwrap();
    world.auth.authenticate(&phone.access_token).await.unwrap();
    world
        .auth
        .forgot_password("alice@example.com")
        .await
        .unwrap();
    let (_, payload) = outbox(&world.pool).await.pop().unwrap();
    let link = payload["params"]["link"].as_str().unwrap();
    let token = link.split_once("?token=").unwrap().1;
    world
        .auth
        .reset_password(token, "battery staple")
        .await
        .unwrap();
    assert_eq!(
        world.auth.authenticate(&phone.access_token).await,
        Err(AuthError::Unauthorized),
        "a token issued before the reset"
    );
    // Tokens carry whole seconds: one issued in a later second than the reset works.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let again = world
        .auth
        .login_device("alice@example.com", "battery staple", phone.device_id)
        .await
        .unwrap();
    assert_eq!(
        world.auth.authenticate(&again.access_token).await,
        Ok(caller(&again))
    );
}

#[tokio::test]
async fn profiles_show_handles_and_only_the_owner_sees_the_email() {
    let world = world().await;
    let alice = verified(&world, "alice").await;
    let phone = world
        .auth
        .register_device(&alice, Uuid::now_v7(), "Phone", "ios")
        .await
        .unwrap();
    let own = world.auth.own_profile(caller(&phone)).await.unwrap();
    assert_eq!(own.handle, "alice");
    assert_eq!(own.display_name, "ALICE");
    assert_eq!(own.email.as_deref(), Some("alice@example.com"));
    let renamed = world
        .auth
        .set_display_name(caller(&phone), "  Alice A. ")
        .await
        .unwrap();
    assert_eq!(renamed.display_name, "Alice A.");
    assert!(matches!(
        world.auth.set_display_name(caller(&phone), "").await,
        Err(AuthError::Invalid(_))
    ));

    let found = world.auth.lookup("@ALICE").await.unwrap();
    assert_eq!(found.user_id, phone.user_id);
    assert_eq!(found.handle, "alice");
    assert_eq!(found.display_name, "Alice A.");
    assert_eq!(
        world.auth.lookup("nobody").await,
        Err(AuthError::UserNotFound)
    );
    // Unverified accounts are not users yet.
    world
        .auth
        .signup(Signup {
            email: "carol@example.com".to_owned(),
            password: "correct horse".to_owned(),
            handle: "carol".to_owned(),
            display_name: "Carol".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(
        world.auth.lookup("carol").await,
        Err(AuthError::UserNotFound)
    );
    assert!(matches!(
        world.auth.lookup("no").await,
        Err(AuthError::Invalid(_))
    ));
}

#[tokio::test]
async fn concurrent_refreshes_with_one_token_leave_exactly_one_live_branch() {
    let world = world().await;
    let registration = verified(&world, "alice").await;
    let first = world
        .auth
        .register_device(&registration, Uuid::now_v7(), "Phone", "ios")
        .await
        .unwrap();
    let auth = Arc::new(world.auth.clone());
    let attempts: Vec<_> = (0..5)
        .map(|_| {
            let (auth, token) = (Arc::clone(&auth), first.refresh_token.clone());
            tokio::spawn(async move { auth.refresh(&token).await })
        })
        .collect();
    let mut issued = Vec::new();
    for attempt in attempts {
        issued.push(attempt.await.unwrap().unwrap().refresh_token);
    }
    let live: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT token_hash FROM auth.refresh_token \
         WHERE rotated_to IS NULL AND revoked_at IS NULL",
    )
    .fetch_all(&world.pool)
    .await
    .unwrap();
    assert_eq!(live.len(), 1, "one branch lives");
    assert!(
        issued
            .iter()
            .any(|token| refresh_hash(token).as_slice() == live[0].as_slice())
    );
}

#[tokio::test]
async fn a_retried_registration_returns_the_same_device() {
    let world = world().await;
    let registration = verified(&world, "alice").await;
    let key = Uuid::now_v7();
    let first = world
        .auth
        .register_device(&registration, key, "Phone", "ios")
        .await
        .unwrap();
    let retried = world
        .auth
        .register_device(&registration, key, "Phone", "ios")
        .await
        .unwrap();
    assert_eq!(retried.device_id, first.device_id);
    assert_ne!(retried.refresh_token, first.refresh_token);
    assert_eq!(
        world.auth.refresh(&first.refresh_token).await,
        Err(AuthError::Unauthorized),
        "the lost answer's tokens are revoked"
    );
    let announced = outbox(&world.pool)
        .await
        .into_iter()
        .filter(|(kind, _)| kind == "device.registered")
        .count();
    assert_eq!(announced, 1);

    // At the limit a retry still answers, and a new registration does not.
    for index in 1..MAX_ACTIVE_DEVICES {
        world
            .auth
            .register_device(
                &registration,
                Uuid::now_v7(),
                &format!("Device {index}"),
                "web",
            )
            .await
            .unwrap();
    }
    let at_limit = world
        .auth
        .register_device(&registration, key, "Phone", "ios")
        .await
        .unwrap();
    assert_eq!(at_limit.device_id, first.device_id);
    assert_eq!(
        world
            .auth
            .register_device(&registration, Uuid::now_v7(), "One too many", "web")
            .await,
        Err(AuthError::TooManyDevices)
    );

    // The key belongs to this user only, and a revoked device's key stays spent.
    let bob = verified(&world, "bob").await;
    let bobs = world
        .auth
        .register_device(&bob, key, "Phone", "ios")
        .await
        .unwrap();
    assert_ne!(bobs.device_id, first.device_id);
    world
        .auth
        .revoke_device(caller(&at_limit), first.device_id)
        .await
        .unwrap();
    assert_eq!(
        world
            .auth
            .register_device(&registration, key, "Phone", "ios")
            .await,
        Err(AuthError::DeviceNotFound)
    );
}

#[tokio::test]
async fn a_password_reset_makes_a_registration_token_worthless() {
    let world = world().await;
    let registration = verified(&world, "alice").await;
    world
        .auth
        .forgot_password("alice@example.com")
        .await
        .unwrap();
    let (_, payload) = outbox(&world.pool).await.pop().unwrap();
    let link = payload["params"]["link"].as_str().unwrap();
    let token = link.split_once("?token=").unwrap().1;
    world
        .auth
        .reset_password(token, "battery staple")
        .await
        .unwrap();
    assert_eq!(
        world
            .auth
            .register_device(&registration, Uuid::now_v7(), "Phone", "ios")
            .await,
        Err(AuthError::Unauthorized)
    );
    let fresh = world
        .auth
        .login("alice@example.com", "battery staple")
        .await
        .unwrap();
    let claims = LinkSigner::new(LINK_KEY.to_vec())
        .unwrap()
        .verify(&fresh.token, Purpose::Register, 0)
        .unwrap();
    let password_hash: String = sqlx::query_scalar("SELECT password_hash FROM auth.user_account")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(claims.pw, Some(password_stamp(&password_hash)));
    world
        .auth
        .register_device(&fresh.token, Uuid::now_v7(), "Phone", "ios")
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reuse_detection_leaves_no_live_token_while_the_thief_keeps_refreshing() {
    let world = world().await;
    let registration = verified(&world, "alice").await;
    for round in 0..20_u64 {
        let first = world
            .auth
            .register_device(&registration, Uuid::now_v7(), "Phone", "ios")
            .await
            .unwrap();
        let second = world.auth.refresh(&first.refresh_token).await.unwrap();
        let third = world.auth.refresh(&second.refresh_token).await.unwrap();
        after_the_grace_window(&world.pool).await;

        let done = Arc::new(AtomicBool::new(false));
        let thief = {
            let (auth, done) = (world.auth.clone(), Arc::clone(&done));
            tokio::spawn(async move {
                let mut latest = third.refresh_token;
                while !done.load(Ordering::SeqCst) {
                    match auth.refresh(&latest).await {
                        Ok(next) => latest = next.refresh_token,
                        Err(AuthError::Unauthorized) => break,
                        Err(other) => panic!("unexpected {other:?}"),
                    }
                }
                latest
            })
        };
        tokio::time::sleep(Duration::from_millis(round % 5 * 4)).await;
        // The user presents a token rotated long ago: reuse.
        assert_eq!(
            world.auth.refresh(&first.refresh_token).await,
            Err(AuthError::Unauthorized)
        );
        done.store(true, Ordering::SeqCst);
        let latest = thief.await.unwrap();
        assert_eq!(
            world.auth.refresh(&latest).await,
            Err(AuthError::Unauthorized),
            "round {round}: the thief's newest token must be dead"
        );
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM auth.refresh_token \
             WHERE device_id = $1 AND rotated_to IS NULL AND revoked_at IS NULL",
        )
        .bind(first.device_id)
        .fetch_one(&world.pool)
        .await
        .unwrap();
        assert_eq!(live, 0, "round {round}: no live token is left");
        world
            .auth
            .revoke_device(caller(&first), first.device_id)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refreshing_while_the_device_is_revoked_never_fails_with_a_store_error() {
    let world = world().await;
    let registration = verified(&world, "alice").await;
    for round in 0..20 {
        let tokens = world
            .auth
            .register_device(&registration, Uuid::now_v7(), "Phone", "ios")
            .await
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let refreshing = {
            let (auth, barrier) = (world.auth.clone(), Arc::clone(&barrier));
            let token = tokens.refresh_token.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                auth.refresh(&token).await
            })
        };
        let (owner, device_id) = (caller(&tokens), tokens.device_id);
        let revoking = {
            let (auth, barrier) = (world.auth.clone(), Arc::clone(&barrier));
            tokio::spawn(async move {
                barrier.wait().await;
                auth.revoke_device(owner, device_id).await
            })
        };
        let refreshed = refreshing.await.unwrap();
        assert!(
            matches!(refreshed, Ok(_) | Err(AuthError::Unauthorized)),
            "round {round}: refresh gave {refreshed:?}"
        );
        assert_eq!(revoking.await.unwrap(), Ok(()), "round {round}");
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM auth.refresh_token \
             WHERE device_id = $1 AND revoked_at IS NULL",
        )
        .bind(tokens.device_id)
        .fetch_one(&world.pool)
        .await
        .unwrap();
        assert_eq!(
            live, 0,
            "round {round}: the revoked device has no live token"
        );
    }
}
