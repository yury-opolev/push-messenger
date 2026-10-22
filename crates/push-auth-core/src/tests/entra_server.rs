//! The whole service from its variables, Entra ID only, on real sockets with PostgreSQL
//! and Redis (Docker must be running) and the stub issuer.

use std::net::SocketAddr;
use std::time::Duration;

use serde_json::json;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::Config;
use crate::entra::testing::StubOpenId;

pub(super) async fn ready(client: &reqwest::Client, health: SocketAddr) {
    for _ in 0..240 {
        let status = client
            .get(format!("http://{health}/readyz"))
            .send()
            .await
            .map(|answer| answer.status().as_u16());
        if matches!(status, Ok(200)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("auth-svc did not become ready within 60 s");
}

#[tokio::test]
async fn the_service_starts_entra_only_and_signs_in_over_a_socket() {
    service_offer("entra").await;
}

#[tokio::test]
async fn the_service_exposes_only_explicitly_enabled_app_and_user_routes() {
    service_offer("entra-app").await;
    service_offer("entra,entra-app").await;
}

async fn service_offer(providers: &str) {
    let postgres = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start Postgres (is Docker running?)");
    let redis = Redis::default()
        .with_tag("7-alpine")
        .start()
        .await
        .expect("start Redis");
    // A password device from before the switch to Entra ID only.
    let url = format!(
        "postgres://postgres:postgres@127.0.0.1:{}/postgres",
        postgres.get_host_port_ipv4(5432).await.unwrap()
    );
    crate::db::migrate_database(&url).await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let before = super::service(pool.clone(), crate::service::SignIn::default());
    let alice = super::password_user(&before, &pool, "alice", 1).await;
    let stub = StubOpenId::start().await;
    let mut variables: Vec<(String, String)> = vec![
        (
            "PUSH_DATABASE_URL".to_owned(),
            format!(
                "postgres://postgres:postgres@127.0.0.1:{}/postgres",
                postgres.get_host_port_ipv4(5432).await.unwrap()
            ),
        ),
        (
            "PUSH_REDIS_URL".to_owned(),
            format!(
                "redis://127.0.0.1:{}",
                redis.get_host_port_ipv4(6379).await.unwrap()
            ),
        ),
        (
            "PUSH_AUTH_PUBLIC_URL".to_owned(),
            "http://push.local".to_owned(),
        ),
        (
            "PUSH_AUTH_TOKEN_AUDIENCE".to_owned(),
            "ws://push.local/v1/socket".to_owned(),
        ),
        // base64 of 32 bytes of 0x07 and of 0x09.
        (
            "PUSH_AUTH_SIGNING_KEY".to_owned(),
            "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=".to_owned(),
        ),
        (
            "PUSH_AUTH_LINK_KEY".to_owned(),
            "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=".to_owned(),
        ),
    ];
    variables.extend(stub.variables());
    variables.retain(|(key, _)| key != "PUSH_AUTH_PROVIDERS");
    variables.push(("PUSH_AUTH_PROVIDERS".to_owned(), providers.to_owned()));
    let config = Config::from_lookup(|name| {
        variables
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (addr, health_addr) = (http.local_addr().unwrap(), health.local_addr().unwrap());
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(crate::serve(config, http, health, shutdown.clone()));

    let client = reqwest::Client::new();
    ready(&client, health_addr).await;
    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM auth.device WHERE device_id = $1")
            .bind(alice[0].device_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        revoked,
        "ready only after the password devices are signed out"
    );
    let signed_in = client
        .post(format!("http://{addr}/v1/auth/entra"))
        .json(&json!({"token": stub.token("55555555-0000-4000-8000-000000000005")}))
        .send()
        .await
        .unwrap();
    let user_enabled = providers.split(',').any(|provider| provider == "entra");
    assert_eq!(signed_in.status(), if user_enabled { 200 } else { 404 });
    let mut app_claims = stub.user_claims(
        crate::entra::testing::TENANT,
        "55555555-0000-4000-8000-000000000005",
    );
    app_claims.as_object_mut().unwrap().remove("scp");
    app_claims["idtyp"] = json!("app");
    let app = client.post(format!("http://{addr}/v1/auth/entra-app"))
        .json(&json!({"token": crate::entra::testing::sign(crate::entra::testing::KEY_A, &app_claims)}))
        .send().await.unwrap();
    assert_eq!(
        app.status(),
        if providers.contains("entra-app") {
            403
        } else {
            404
        }
    );
    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM auth.user_account")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(accounts, if user_enabled { 2 } else { 1 });
    let login = client
        .post(format!("http://{addr}/v1/auth/login"))
        .json(&json!({"email": "a@example.com", "password": "correct horse"}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 404);
    assert!(stub.jwks_fetches() >= 1);

    shutdown.cancel();
    server.await.unwrap().unwrap();
}
