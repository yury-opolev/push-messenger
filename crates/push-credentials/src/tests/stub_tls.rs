//! The stub issuer served over HTTPS, as the DEV ONLY in-cluster rehearsal runs it.

use std::time::Duration;

use crate::stub::{StubIssuer, StubPrincipal, serve_tls};
use crate::testing::TestCa;
use crate::{TokenSource, WorkloadIdentity};

const TENANT: &str = "11111111-2222-3333-4444-555555555555";
const SCOPE: &str = "https://redis.azure.com/.default";

#[tokio::test]
async fn the_issuer_serves_tokens_over_tls_to_clients_trusting_the_test_ca() {
    crate::install_crypto_provider();
    let ca = TestCa::new();
    let (cert, key) = ca.server_for(&["localhost"]);
    let principals = vec![StubPrincipal {
        client_id: "client-a".to_owned(),
        object_id: "oid-a".to_owned(),
    }];
    // `None`: any non-empty assertion, as a rotating service-account token in a cluster.
    let (issuer, router) = StubIssuer::new(TENANT, None, principals, Duration::from_secs(60), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { serve_tls(listener, router, &cert, &key).await });
    let authority = format!("https://localhost:{port}/");
    let issuer = issuer.with_authority(authority.clone());
    assert_eq!(issuer.authority(), authority);

    let assertion =
        std::env::temp_dir().join(format!("push-stub-tls-{}.jwt", uuid::Uuid::now_v7()));
    std::fs::write(&assertion, "any-service-account-token").unwrap();
    let trusting = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(ca.pem().as_bytes()).unwrap())
        .build()
        .unwrap();
    let source =
        WorkloadIdentity::new(trusting, &authority, TENANT, "client-a", &assertion).unwrap();
    let token = source.fetch(SCOPE).await.unwrap();
    assert_eq!(token.object_id().as_deref(), Some("oid-a"));
    assert_eq!(issuer.issued().len(), 1);

    // A client without the test CA refuses the issuer's certificate.
    let untrusting = WorkloadIdentity::new(
        reqwest::Client::new(),
        &authority,
        TENANT,
        "client-a",
        &assertion,
    )
    .unwrap();
    assert!(untrusting.fetch(SCOPE).await.is_err());
    assert_eq!(issuer.issued().len(), 1);
    let _ = std::fs::remove_file(&assertion);
}
