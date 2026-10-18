//! The TLS test servers accept clients that trust the test CA, and nobody else.

use crate::testing::{TestCa, tls_postgres, tls_redis};

#[tokio::test]
async fn the_tls_servers_trust_only_the_test_ca() {
    crate::install_crypto_provider();
    let ca = TestCa::new();
    let postgres = tls_postgres(&ca).await;
    let admin = postgres.admin(&ca).await;
    let one: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(&admin)
        .await
        .unwrap();
    assert_eq!(one, 1);
    let other = TestCa::new();
    let refused = postgres.try_admin(&other).await.unwrap_err();
    assert!(
        crate::is_certificate_error(&refused),
        "an untrusted CA is refused as a certificate error: {refused:?}"
    );

    let redis = tls_redis(&ca).await;
    let mut connection = redis.admin(&ca).await;
    let pong: String = redis::cmd("PING")
        .query_async(&mut connection)
        .await
        .unwrap();
    assert_eq!(pong, "PONG");
}
