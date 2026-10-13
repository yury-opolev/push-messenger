//! Signup, email verification, login and password reset on a real PostgreSQL (Docker
//! must be running). Each test starts a container of its own; they run in parallel.

use std::time::Duration;

use push_auth::links::{LinkClaims, LinkSigner, Purpose, password_stamp};
use push_auth::password::Passwords;
use push_auth::tokens::AccessTokens;
use push_auth::{Auth, AuthError, Settings, Signup, migrate_database};
use serde_json::Value;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use uuid::Uuid;

const LINK_KEY: [u8; 32] = [3; 32];

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
    let auth = Auth::new(pool.clone(), settings(Passwords::new(64, 1, 1).unwrap()));
    World {
        _container: container,
        auth,
        pool,
    }
}

fn settings(passwords: Passwords) -> Settings {
    Settings {
        public_url: "https://api.example.com/".to_owned(),
        passwords,
        links: LinkSigner::new(LINK_KEY.to_vec()).unwrap(),
        tokens: AccessTokens::from_seed(&[5; 32], "wss://api.example.com/v1/socket").unwrap(),
        hash_concurrency: 4,
        hash_queue: 64,
        hash_wait: Duration::from_secs(5),
    }
}

fn signup(email: &str, handle: &str) -> Signup {
    Signup {
        email: email.to_owned(),
        password: "correct horse".to_owned(),
        handle: handle.to_owned(),
        display_name: "Alice".to_owned(),
    }
}

/// The outbox, oldest first: `(kind, payload)`.
async fn outbox(pool: &PgPool) -> Vec<(String, Value)> {
    sqlx::query_as("SELECT kind, payload FROM auth.outbox ORDER BY created_at, event_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// The token of the newest email link sent to `to`.
async fn last_link_token(pool: &PgPool, to: &str) -> String {
    let link = outbox(pool)
        .await
        .into_iter()
        .rev()
        .find(|(kind, payload)| kind == "email.send" && payload["to"] == to)
        .map(|(_, payload)| payload["params"]["link"].as_str().unwrap().to_owned())
        .expect("an email with a link");
    link.split_once("?token=").unwrap().1.to_owned()
}

#[tokio::test]
async fn signup_writes_a_verification_email_and_the_account_waits_for_it() {
    let world = world().await;
    world
        .auth
        .signup(signup(" alice@Example.com ", "@Alice"))
        .await
        .unwrap();

    let events = outbox(&world.pool).await;
    assert_eq!(events.len(), 1);
    let (kind, payload) = &events[0];
    assert_eq!(kind, "email.send");
    assert_eq!(payload["to"], "alice@example.com");
    assert_eq!(payload["template"], "verify_email");
    assert_eq!(payload["params"]["handle"], "alice");
    assert_eq!(payload["params"]["expires_in_minutes"], 30);
    let link = payload["params"]["link"].as_str().unwrap();
    assert!(
        link.starts_with("https://api.example.com/v1/auth/verify?token="),
        "{link}"
    );
    assert_eq!(
        world.auth.login("alice@example.com", "correct horse").await,
        Err(AuthError::EmailNotVerified)
    );
}

#[tokio::test]
async fn the_link_verifies_once_announces_the_user_and_opens_login() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    let token = last_link_token(&world.pool, "alice@example.com").await;
    world.auth.verify_email(&token).await.unwrap();
    world.auth.verify_email(&token).await.unwrap();

    let events = outbox(&world.pool).await;
    let verified: Vec<_> = events
        .iter()
        .filter(|(kind, _)| kind == "user.verified")
        .collect();
    assert_eq!(verified.len(), 1, "announced once: {events:?}");
    let user_id = verified[0].1["user_id"].as_str().unwrap().to_owned();

    let registration = world
        .auth
        .login("ALICE@example.com", "correct horse")
        .await
        .unwrap();
    assert_eq!(registration.expires_in_s, 300);
    let signer = LinkSigner::new(LINK_KEY.to_vec()).unwrap();
    let claims = signer
        .verify(&registration.token, Purpose::Register, 0)
        .unwrap();
    assert_eq!(claims.sub.to_string(), user_id);
}

#[tokio::test]
async fn signing_up_again_takes_over_an_unverified_account_and_says_nothing() {
    let world = world().await;
    // Someone signs up first with Alice's address and a password of their own.
    world
        .auth
        .signup(Signup {
            password: "mallory's guess".to_owned(),
            display_name: "Mallory".to_owned(),
            ..signup("alice@example.com", "mallory")
        })
        .await
        .unwrap();
    let first_link = last_link_token(&world.pool, "alice@example.com").await;
    // Alice signs up: the same answer, and her values replace the first ones.
    world
        .auth
        .signup(signup("Alice@example.com", "alice"))
        .await
        .unwrap();
    assert_eq!(
        world.auth.verify_email(&first_link).await,
        Err(AuthError::LinkInvalid),
        "a link sent before the takeover verifies nothing"
    );
    let token = last_link_token(&world.pool, "alice@example.com").await;
    world.auth.verify_email(&token).await.unwrap();
    assert_eq!(
        world
            .auth
            .login("alice@example.com", "mallory's guess")
            .await,
        Err(AuthError::InvalidCredentials)
    );
    world
        .auth
        .login("alice@example.com", "correct horse")
        .await
        .unwrap();
    // Verified: the same answer, an "account exists" email, and nothing changes.
    world
        .auth
        .signup(Signup {
            password: "another password".to_owned(),
            ..signup("alice@example.com", "alice3")
        })
        .await
        .unwrap();
    world
        .auth
        .login("alice@example.com", "correct horse")
        .await
        .unwrap();

    let emails: Vec<(String, String)> = outbox(&world.pool)
        .await
        .into_iter()
        .filter(|(kind, _)| kind == "email.send")
        .map(|(_, payload)| {
            (
                payload["template"].as_str().unwrap().to_owned(),
                payload["params"]["handle"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        emails,
        [
            ("verify_email".to_owned(), "mallory".to_owned()),
            ("verify_email".to_owned(), "alice".to_owned()),
            ("account_exists".to_owned(), "alice".to_owned()),
        ]
    );
    let accounts: Vec<(String, String)> =
        sqlx::query_as("SELECT handle::text, display_name FROM auth.user_account")
            .fetch_all(&world.pool)
            .await
            .unwrap();
    assert_eq!(accounts, [("alice".to_owned(), "Alice".to_owned())]);
}

#[tokio::test]
async fn a_taken_handle_and_invalid_input_are_reported() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    assert_eq!(
        world.auth.signup(signup("bob@example.com", "@ALICE")).await,
        Err(AuthError::HandleTaken)
    );
    assert_eq!(
        world
            .auth
            .signup(signup("alice@example.com", "alice"))
            .await,
        Err(AuthError::HandleTaken),
        "the account's own handle too: the answer does not depend on the email"
    );
    let short_password = Signup {
        password: "short".to_owned(),
        ..signup("bob@example.com", "bob")
    };
    let error = world.auth.signup(short_password).await.unwrap_err();
    assert_eq!(error.code(), "invalid");
    assert_eq!(error.status(), 400);
    assert!(
        world
            .auth
            .signup(signup("not-an-email", "bob"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn forged_expired_and_mismatched_links_are_refused() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    let token = last_link_token(&world.pool, "alice@example.com").await;
    let user_id = LinkSigner::new(LINK_KEY.to_vec())
        .unwrap()
        .verify(&token, Purpose::Verify, 0)
        .unwrap()
        .sub;
    let signer = LinkSigner::new(LINK_KEY.to_vec()).unwrap();
    let password_hash: String =
        sqlx::query_scalar("SELECT password_hash FROM auth.user_account WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&world.pool)
            .await
            .unwrap();
    let stamp = password_stamp(&password_hash);
    let claims = |exp: i64, email: &str, purpose: Purpose| LinkClaims {
        sub: user_id,
        purpose,
        exp,
        email: Some(email.to_owned()),
        pw: Some(stamp.clone()),
    };
    let far = i64::MAX;
    let cases = [
        (
            signer.sign(&claims(1, "alice@example.com", Purpose::Verify)),
            AuthError::LinkExpired,
        ),
        (
            signer.sign(&claims(far, "old@example.com", Purpose::Verify)),
            AuthError::LinkInvalid,
        ),
        (
            signer.sign(&claims(far, "alice@example.com", Purpose::Register)),
            AuthError::LinkInvalid,
        ),
        (
            signer.sign(&LinkClaims {
                pw: None,
                ..claims(far, "alice@example.com", Purpose::Verify)
            }),
            AuthError::LinkInvalid,
        ),
        (
            signer.sign(&LinkClaims {
                pw: Some(password_stamp("another hash")),
                ..claims(far, "alice@example.com", Purpose::Verify)
            }),
            AuthError::LinkInvalid,
        ),
        (
            LinkSigner::new(vec![4; 32]).unwrap().sign(&claims(
                far,
                "alice@example.com",
                Purpose::Verify,
            )),
            AuthError::LinkInvalid,
        ),
        ("garbage".to_owned(), AuthError::LinkInvalid),
    ];
    for (token, expected) in cases {
        assert_eq!(world.auth.verify_email(&token).await, Err(expected));
    }
    let unverified: bool = sqlx::query_scalar(
        "SELECT email_verified_at IS NULL FROM auth.user_account WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert!(unverified);
}

#[tokio::test]
async fn an_unknown_email_and_a_wrong_password_are_the_same_error() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    let token = last_link_token(&world.pool, "alice@example.com").await;
    world.auth.verify_email(&token).await.unwrap();
    for (email, password) in [
        ("alice@example.com", "wrong horse"),
        ("nobody@example.com", "correct horse"),
        ("not an email", "correct horse"),
    ] {
        assert_eq!(
            world.auth.login(email, password).await,
            Err(AuthError::InvalidCredentials),
            "{email}"
        );
    }
}

#[tokio::test]
async fn resending_writes_a_new_email_only_for_an_unverified_account() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    world
        .auth
        .resend_verification("alice@example.com")
        .await
        .unwrap();
    world
        .auth
        .resend_verification("nobody@example.com")
        .await
        .unwrap();
    assert_eq!(outbox(&world.pool).await.len(), 2);
    let token = last_link_token(&world.pool, "alice@example.com").await;
    world.auth.verify_email(&token).await.unwrap();
    world
        .auth
        .resend_verification("alice@example.com")
        .await
        .unwrap();
    let kinds: Vec<String> = outbox(&world.pool)
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(kinds, ["email.send", "email.send", "user.verified"]);
}

#[tokio::test]
async fn a_reset_link_sets_the_password_once_and_announces_it() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    let token = last_link_token(&world.pool, "alice@example.com").await;
    world.auth.verify_email(&token).await.unwrap();

    world
        .auth
        .forgot_password("nobody@example.com")
        .await
        .unwrap();
    world
        .auth
        .forgot_password("alice@example.com")
        .await
        .unwrap();
    let (kind, payload) = outbox(&world.pool).await.pop().unwrap();
    assert_eq!(kind, "email.send");
    assert_eq!(payload["template"], "reset_password");
    let link = payload["params"]["link"].as_str().unwrap();
    assert!(
        link.starts_with("https://api.example.com/v1/auth/password/reset?token="),
        "{link}"
    );
    let reset = last_link_token(&world.pool, "alice@example.com").await;

    assert!(matches!(
        world.auth.reset_password(&reset, "short").await,
        Err(AuthError::Invalid(_))
    ));
    world
        .auth
        .reset_password(&reset, "battery staple")
        .await
        .unwrap();
    assert_eq!(
        world.auth.reset_password(&reset, "another password").await,
        Err(AuthError::LinkInvalid),
        "a used link"
    );
    assert_eq!(
        world.auth.login("alice@example.com", "correct horse").await,
        Err(AuthError::InvalidCredentials)
    );
    world
        .auth
        .login("alice@example.com", "battery staple")
        .await
        .unwrap();
    let (kind, _) = outbox(&world.pool).await.pop().unwrap();
    assert_eq!(kind, "user.password_reset");
}

#[tokio::test]
async fn login_replaces_a_hash_made_with_outdated_parameters() {
    let world = world().await;
    let old = Auth::new(
        world.pool.clone(),
        settings(Passwords::new(32, 1, 1).unwrap()),
    );
    old.signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    let token = last_link_token(&world.pool, "alice@example.com").await;
    world.auth.verify_email(&token).await.unwrap();
    let stored = || async {
        sqlx::query_scalar::<_, String>("SELECT password_hash FROM auth.user_account")
            .fetch_one(&world.pool)
            .await
            .unwrap()
    };
    assert!(stored().await.starts_with("$argon2id$v=19$m=32,t=1,p=1$"));
    world
        .auth
        .login("alice@example.com", "correct horse")
        .await
        .unwrap();
    assert!(stored().await.starts_with("$argon2id$v=19$m=64,t=1,p=1$"));
}

#[tokio::test]
async fn new_ids_are_time_ordered() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    let user_id: Uuid = sqlx::query_scalar("SELECT user_id FROM auth.user_account")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(user_id.get_version_num(), 7);
}

#[tokio::test]
async fn a_reset_also_verifies_the_email() {
    let world = world().await;
    world
        .auth
        .signup(signup("alice@example.com", "alice"))
        .await
        .unwrap();
    let verify_link = last_link_token(&world.pool, "alice@example.com").await;
    world
        .auth
        .forgot_password("alice@example.com")
        .await
        .unwrap();
    let reset = last_link_token(&world.pool, "alice@example.com").await;
    world
        .auth
        .reset_password(&reset, "battery staple")
        .await
        .unwrap();
    world
        .auth
        .login("alice@example.com", "battery staple")
        .await
        .unwrap();
    let kinds: Vec<String> = outbox(&world.pool)
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "email.send",
            "email.send",
            "user.verified",
            "user.password_reset"
        ]
    );
    assert_eq!(
        world.auth.verify_email(&verify_link).await,
        Err(AuthError::LinkInvalid),
        "the old verification link binds the old password"
    );

    // A second reset of the now verified account announces no second verification.
    world
        .auth
        .forgot_password("alice@example.com")
        .await
        .unwrap();
    let reset = last_link_token(&world.pool, "alice@example.com").await;
    world
        .auth
        .reset_password(&reset, "correct horse")
        .await
        .unwrap();
    let verified = outbox(&world.pool)
        .await
        .into_iter()
        .filter(|(kind, _)| kind == "user.verified")
        .count();
    assert_eq!(verified, 1);
}

#[tokio::test]
async fn a_login_flood_fails_fast_with_a_retryable_error() {
    let world = world().await;
    let narrow = Auth::new(
        world.pool.clone(),
        Settings {
            hash_concurrency: 1,
            hash_queue: 0,
            hash_wait: Duration::from_millis(50),
            ..settings(Passwords::owasp())
        },
    );
    let attempts: Vec<_> = (0..8)
        .map(|_| {
            let auth = narrow.clone();
            tokio::spawn(async move { auth.login("nobody@example.com", "correct horse").await })
        })
        .collect();
    let mut busy = 0;
    for attempt in attempts {
        match attempt.await.unwrap() {
            Err(AuthError::InvalidCredentials) => {}
            Err(AuthError::Unavailable) => busy += 1,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!((1..8).contains(&busy), "{busy} of 8 refused");
    assert!(AuthError::Unavailable.retryable());
}
