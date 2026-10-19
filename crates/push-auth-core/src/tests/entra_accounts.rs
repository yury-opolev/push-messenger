//! Entra ID accounts in the service (spec AC4, AC5): found by `(tid, oid)`, created once,
//! never linked by address; registration tokens bound to the identity; the session's
//! maximum age.

use uuid::Uuid;

use super::{LINK_KEY, Offer, World, world};
use crate::devices::{Caller, Tokens};
use crate::entra::EntraIdentity;
use crate::error::AuthError;
use crate::links::{LinkClaims, LinkSigner, Purpose, password_stamp};
use crate::service::{SignIn, Signup, unix_now};

const TENANT: Uuid = Uuid::from_u128(0x33333333_0000_4000_8000_000000000003);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x44444444_0000_4000_8000_000000000004);

fn person(object: u128, email: &str) -> EntraIdentity {
    EntraIdentity {
        tenant_id: TENANT,
        object_id: Uuid::from_u128(object),
        name: Some("Erin Example".to_owned()),
        email: Some(email.to_owned()),
    }
}

async fn register(world: &World, identity: &EntraIdentity) -> Tokens {
    let signed_in = world.auth.sign_in_entra(identity, None).await.unwrap();
    world
        .auth
        .register_device(
            &signed_in.registration.token,
            Uuid::now_v7(),
            "Laptop",
            "web",
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn the_first_sign_in_creates_an_active_account_without_email_or_password() {
    let world = world(Offer::Entra).await;
    let signed_in = world
        .auth
        .sign_in_entra(&person(1, "erin@contoso.test"), None)
        .await
        .unwrap();
    assert_eq!(signed_in.registration.expires_in_s, 300);
    let (email, password, active, handle, name): (
        Option<String>,
        Option<String>,
        bool,
        String,
        String,
    ) = sqlx::query_as(
        "SELECT email::text, password_hash, email_verified_at IS NOT NULL, handle::text, \
         display_name FROM auth.user_account WHERE user_id = $1",
    )
    .bind(signed_in.user_id)
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert_eq!((email, password, active), (None, None, true));
    assert!(
        handle.len() == 11 && handle.starts_with('u'),
        "a generated handle: {handle}"
    );
    assert_eq!(name, "Erin Example");
    let kinds: Vec<String> = world
        .outbox()
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(kinds, ["user.verified"]);

    let tokens = world
        .auth
        .register_device(
            &signed_in.registration.token,
            Uuid::now_v7(),
            "Laptop",
            "web",
        )
        .await
        .unwrap();
    assert_eq!(tokens.user_id, signed_in.user_id);
    let own = world
        .auth
        .own_profile(Caller {
            user_id: tokens.user_id,
            device_id: tokens.device_id,
        })
        .await
        .unwrap();
    assert_eq!(own.email.as_deref(), Some("erin@contoso.test"));
    assert_eq!(
        world.auth.lookup(&own.handle).await.unwrap().user_id,
        tokens.user_id,
        "found by handle like any user"
    );
}

#[tokio::test]
async fn the_same_identity_is_always_the_same_user_and_addresses_never_link() {
    let world = world(Offer::Both).await;
    let first = world
        .auth
        .sign_in_entra(&person(1, "erin@contoso.test"), None)
        .await
        .unwrap()
        .user_id;
    // A new UPN, name and address: the same user, whose display name stays.
    let renamed = EntraIdentity {
        name: Some("Erin Married".to_owned()),
        email: Some("erin.married@contoso.test".to_owned()),
        ..person(1, "")
    };
    assert_eq!(
        world
            .auth
            .sign_in_entra(&renamed, None)
            .await
            .unwrap()
            .user_id,
        first
    );
    let (name, email): (String, Option<String>) = sqlx::query_as(
        "SELECT account.display_name, identity.email FROM auth.user_account AS account \
         JOIN auth.external_identity AS identity USING (user_id) WHERE account.user_id = $1",
    )
    .bind(first)
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert_eq!(
        (name.as_str(), email.as_deref()),
        ("Erin Example", Some("erin.married@contoso.test"))
    );
    // Another Entra ID user with the first one's address: another account.
    let namesake = world
        .auth
        .sign_in_entra(&person(2, "erin@contoso.test"), None)
        .await
        .unwrap()
        .user_id;
    assert_ne!(namesake, first);
    // The same object id in another allowed tenant: another person.
    let elsewhere = EntraIdentity {
        tenant_id: OTHER_TENANT,
        ..person(1, "erin@contoso.test")
    };
    let foreigner = world
        .auth
        .sign_in_entra(&elsewhere, None)
        .await
        .unwrap()
        .user_id;
    assert!(![first, namesake].contains(&foreigner));
    // An email/password account with that address: its own account, never linked.
    world
        .auth
        .signup(Signup {
            email: "erin@contoso.test".to_owned(),
            password: "correct horse".to_owned(),
            handle: "erin".to_owned(),
            display_name: "Erin".to_owned(),
        })
        .await
        .unwrap();
    let password_user: Uuid = sqlx::query_scalar(
        "SELECT user_id FROM auth.user_account WHERE email = 'erin@contoso.test'",
    )
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert!(![first, namesake, foreigner].contains(&password_user));
    let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM auth.external_identity")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(identities, 3);
    assert_eq!(
        world
            .auth
            .sign_in_entra(&person(1, "x@contoso.test"), None)
            .await
            .unwrap()
            .user_id,
        first
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_requested_handle_is_used_once_and_concurrent_first_sign_ins_make_one_account() {
    let world = world(Offer::Entra).await;
    let erin = world
        .auth
        .sign_in_entra(&person(1, "e@contoso.test"), Some("@Erin"))
        .await
        .unwrap();
    let handle: String =
        sqlx::query_scalar("SELECT handle::text FROM auth.user_account WHERE user_id = $1")
            .bind(erin.user_id)
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert_eq!(handle, "erin");
    assert_eq!(
        world
            .auth
            .sign_in_entra(&person(2, "f@contoso.test"), Some("erin"))
            .await
            .unwrap_err(),
        AuthError::HandleTaken
    );
    assert!(matches!(
        world
            .auth
            .sign_in_entra(&person(2, "f@contoso.test"), Some("x"))
            .await,
        Err(AuthError::Invalid(_))
    ));
    // Later sign-ins ignore the handle, even an invalid one.
    assert_eq!(
        world
            .auth
            .sign_in_entra(&person(1, "e@contoso.test"), Some("x"))
            .await
            .unwrap()
            .user_id,
        erin.user_id
    );

    let gus = person(3, "g@contoso.test");
    let attempts: Vec<_> = (0..8)
        .map(|_| {
            let (auth, gus) = (world.auth.clone(), gus.clone());
            tokio::spawn(async move { auth.sign_in_entra(&gus, None).await.unwrap().user_id })
        })
        .collect();
    let mut users = Vec::new();
    for attempt in attempts {
        users.push(attempt.await.unwrap());
    }
    users.sort();
    users.dedup();
    assert_eq!(users.len(), 1, "one account for concurrent first sign-ins");
    let accounts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM auth.external_identity WHERE subject = $1")
            .bind(gus.object_id.to_string())
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert_eq!(accounts, 1);
}

#[tokio::test]
async fn an_entra_device_session_ends_at_its_maximum_age_and_a_new_sign_in_renews_it() {
    let world = world(Offer::Entra).await;
    let erin = person(1, "e@contoso.test");
    let tokens = register(&world, &erin).await;
    let session: Option<i64> =
        sqlx::query_scalar("SELECT session_expires_at FROM auth.device WHERE device_id = $1")
            .bind(tokens.device_id)
            .fetch_one(&world.pool)
            .await
            .unwrap();
    let left = session.expect("an Entra device has a session end") - unix_now();
    assert!((24 * 3600 - 10..=24 * 3600).contains(&left), "{left}");
    assert!(tokens.refresh_expires_in_s <= 24 * 3600);
    let refreshed = world.auth.refresh(&tokens.refresh_token).await.unwrap();

    sqlx::query("UPDATE auth.device SET session_expires_at = $2 WHERE device_id = $1")
        .bind(tokens.device_id)
        .bind(unix_now() - 1)
        .execute(&world.pool)
        .await
        .unwrap();
    assert_eq!(
        world.auth.refresh(&refreshed.refresh_token).await,
        Err(AuthError::SessionExpired)
    );
    // The device is still the user's: an Entra ID sign-in for it renews the session.
    let renewed = world
        .auth
        .sign_in_entra_device(&erin, None, tokens.device_id)
        .await
        .unwrap();
    assert_eq!(
        (renewed.user_id, renewed.device_id),
        (tokens.user_id, tokens.device_id)
    );
    assert!(world.auth.refresh(&renewed.refresh_token).await.is_ok());
    // The new sign-in ended the earlier refresh tokens.
    assert_eq!(
        world.auth.refresh(&refreshed.refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    // Another user's device is not found.
    let other = register(&world, &person(2, "f@contoso.test")).await;
    assert_eq!(
        world
            .auth
            .sign_in_entra_device(&erin, None, other.device_id)
            .await,
        Err(AuthError::DeviceNotFound)
    );
}

#[tokio::test]
async fn an_entra_registration_token_registers_only_for_its_identity_and_provider() {
    let world = world(Offer::Both).await;
    let signed_in = world
        .auth
        .sign_in_entra(&person(1, "e@contoso.test"), None)
        .await
        .unwrap();
    let identity: Uuid = sqlx::query_scalar("SELECT identity_id FROM auth.external_identity")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    let signer = LinkSigner::new(LINK_KEY.to_vec()).unwrap();
    let token = |pw: Option<String>, xid: Option<Uuid>, sx: Option<i64>| {
        signer.sign(&LinkClaims {
            sub: signed_in.user_id,
            purpose: Purpose::Register,
            exp: i64::MAX,
            email: None,
            pw,
            xid,
            sx,
        })
    };
    let later = unix_now() + 3600;
    for refused in [
        token(None, None, Some(later)),
        token(None, Some(identity), None),
        token(None, Some(Uuid::now_v7()), Some(later)),
        token(Some(password_stamp("")), Some(identity), Some(later)),
        token(Some(password_stamp("")), None, None),
    ] {
        assert_eq!(
            world
                .auth
                .register_device(&refused, Uuid::now_v7(), "Laptop", "web")
                .await,
            Err(AuthError::Unauthorized)
        );
    }
    assert!(
        world
            .auth
            .register_device(
                &token(None, Some(identity), Some(later)),
                Uuid::now_v7(),
                "Laptop",
                "web"
            )
            .await
            .is_ok()
    );
    // The same token on a service that no longer offers Entra ID sign-in.
    let password_only = world.auth.clone().with_sign_in(SignIn::default());
    assert_eq!(
        password_only
            .register_device(
                &signed_in.registration.token,
                Uuid::now_v7(),
                "Laptop",
                "web"
            )
            .await,
        Err(AuthError::Unauthorized)
    );
}
