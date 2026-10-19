//! The authenticators and the admission a service offers (spec D3, D4, D6).

use std::time::Duration;

use sqlx::PgPool;

use super::{FakeIdentity, RequireGroup, fake_token, quick, service};
use crate::entra::EntraSignIn;
use crate::entra::testing::StubOpenId;
use crate::service::SignIn;

#[tokio::test]
async fn a_service_offers_entra_and_the_external_authenticator_by_their_ids() {
    // Never connected: nothing here touches the database.
    let pool = PgPool::connect_lazy("postgres://127.0.0.1:1/none").unwrap();
    let stub = StubOpenId::start().await;
    let entra = EntraSignIn::with_timing(stub.settings(), quick()).unwrap();
    let plain = service(pool.clone(), SignIn::default());
    assert!(plain.authenticators().is_empty());
    assert!(plain.authenticator("entra").is_none());
    assert!(plain.admission().is_none());

    let acme = FakeIdentity::new("acme");
    let both = service(
        pool,
        SignIn {
            password: true,
            entra: Some(entra),
        },
    )
    .with_external(acme.clone())
    .with_admission(RequireGroup::new("staff"));
    let ids: Vec<String> = both
        .authenticators()
        .iter()
        .map(|authenticator| authenticator.provider().to_owned())
        .collect();
    assert_eq!(ids, ["entra", "acme"]);
    assert_eq!(
        both.authenticator("acme").unwrap().max_session(),
        Duration::from_secs(2 * 3600)
    );
    assert_eq!(
        both.authenticator("entra").unwrap().max_session(),
        Duration::from_secs(24 * 3600)
    );
    assert!(both.authenticator("other").is_none());
    // The very authenticator the wrapper passed.
    let principal = both
        .authenticator("acme")
        .unwrap()
        .authenticate(&fake_token("https://id.acme.test/", "s-1", "", "staff"))
        .await
        .unwrap();
    assert_eq!(
        (principal.provider.as_str(), principal.subject.as_str()),
        ("acme", "s-1")
    );
    assert_eq!(acme.calls(), 1);
    assert!(both.admission().is_some());
}
