//! External accounts in the service (spec D2, D8, AC2): one account per
//! `(provider, issuer, subject)`, whatever the name or the address; registration tokens
//! that only a still offered provider honours; principals that break the rules refused.

use serde_json::Map;
use uuid::Uuid;

use super::{FakeIdentity, Offer, world};
use crate::error::AuthError;
use crate::identity::Principal;

const ISSUER: &str = "https://id.acme.test/";

fn principal(provider: &str, issuer: &str, subject: &str, name: &str, email: &str) -> Principal {
    Principal {
        provider: provider.to_owned(),
        issuer: issuer.to_owned(),
        subject: subject.to_owned(),
        name: Some(name.to_owned()),
        email: Some(email.to_owned()),
        claims: Map::new(),
    }
}

#[tokio::test]
async fn an_external_principal_gets_one_account_whatever_its_name_or_address() {
    let world = world(Offer::Password).await;
    let auth = world.auth.clone().with_external(FakeIdentity::new("acme"));
    let first = auth
        .sign_in_external(
            &principal("acme", ISSUER, "s-1", "Ann Acme", "ann@acme.test"),
            Some("ann"),
        )
        .await
        .unwrap();
    let tokens = auth
        .register_device(&first.registration.token, Uuid::now_v7(), "Laptop", "web")
        .await
        .unwrap();
    assert!(
        tokens.refresh_expires_in_s <= 2 * 3600,
        "the provider's session"
    );

    let again = auth
        .sign_in_external(
            &principal("acme", ISSUER, "s-1", "Ann Other", "ann.other@acme.test"),
            Some("someone_else"),
        )
        .await
        .unwrap();
    assert_eq!(again.user_id, first.user_id);
    let rows: Vec<(String, String, String, Option<String>)> =
        sqlx::query_as("SELECT provider, issuer, subject, email FROM auth.external_identity")
            .fetch_all(&world.pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        [(
            "acme".to_owned(),
            ISSUER.to_owned(),
            "s-1".to_owned(),
            Some("ann.other@acme.test".to_owned())
        )]
    );
    let (handle, name): (String, String) =
        sqlx::query_as("SELECT handle::text, display_name FROM auth.user_account")
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert_eq!((handle.as_str(), name.as_str()), ("ann", "Ann Acme"));

    let renewed = auth
        .sign_in_external_device(
            &principal("acme", ISSUER, "s-1", "Ann", "ann@acme.test"),
            None,
            tokens.device_id,
        )
        .await
        .unwrap();
    assert_eq!(renewed.device_id, tokens.device_id);
    let kinds: Vec<String> = world
        .outbox()
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(kinds, ["user.verified", "device.registered"]);
}

#[tokio::test]
async fn the_provider_and_the_issuer_are_part_of_the_identity() {
    let world = world(Offer::Both).await;
    let auth = world.auth.clone().with_external(FakeIdentity::new("acme"));
    let mut users = Vec::new();
    for (provider, issuer) in [
        ("acme", ISSUER),
        ("acme", "https://other.acme.test/"),
        ("entra", ISSUER),
    ] {
        let signed_in = auth
            .sign_in_external(&principal(provider, issuer, "s-1", "Ann", "a@x.test"), None)
            .await
            .unwrap();
        users.push(signed_in.user_id);
    }
    users.sort();
    users.dedup();
    assert_eq!(users.len(), 3);
    assert_eq!(world.accounts().await, 3);
}

#[tokio::test]
async fn a_registration_token_of_a_provider_no_longer_offered_registers_nothing() {
    let world = world(Offer::Password).await;
    let auth = world.auth.clone().with_external(FakeIdentity::new("acme"));
    let signed_in = auth
        .sign_in_external(&principal("acme", ISSUER, "s-1", "Ann", "a@x.test"), None)
        .await
        .unwrap();
    // world.auth offers email and password only.
    assert!(matches!(
        world
            .auth
            .register_device(
                &signed_in.registration.token,
                Uuid::now_v7(),
                "Laptop",
                "web"
            )
            .await,
        Err(AuthError::Unauthorized)
    ));
    assert!(matches!(
        world
            .auth
            .sign_in_external(&principal("acme", ISSUER, "s-1", "Ann", "a@x.test"), None)
            .await,
        Err(AuthError::Unauthorized)
    ));
    assert!(
        auth.register_device(
            &signed_in.registration.token,
            Uuid::now_v7(),
            "Laptop",
            "web"
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn a_principal_that_breaks_the_rules_is_refused() {
    let world = world(Offer::Password).await;
    let auth = world.auth.clone().with_external(FakeIdentity::new("acme"));
    let long_subject = "s".repeat(129);
    let long_issuer = "i".repeat(257);
    for (issuer, subject) in [
        (ISSUER, long_subject.as_str()),
        (ISSUER, ""),
        ("", "s-1"),
        (long_issuer.as_str(), "s-1"),
        (ISSUER, "s\n1"),
    ] {
        assert!(
            matches!(
                auth.sign_in_external(&principal("acme", issuer, subject, "Ann", "a@x.test"), None)
                    .await,
                Err(AuthError::Unauthorized)
            ),
            "{issuer:?} {subject:?}"
        );
    }
    // Longest allowed values; a name and an address that do not fit are dropped.
    let mut fits = principal("acme", &"i".repeat(256), &"s".repeat(128), "\u{7}", "");
    fits.email = Some("e".repeat(255));
    let signed_in = auth.sign_in_external(&fits, None).await.unwrap();
    let (name, email): (String, Option<String>) = sqlx::query_as(
        "SELECT account.display_name, identity.email FROM auth.user_account AS account \
         JOIN auth.external_identity AS identity USING (user_id) WHERE user_id = $1",
    )
    .bind(signed_in.user_id)
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert!(
        name.starts_with('u'),
        "the generated handle names it: {name}"
    );
    assert_eq!(email, None);
    assert_eq!(world.accounts().await, 1);
}
