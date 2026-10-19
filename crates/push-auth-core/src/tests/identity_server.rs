//! The whole service through `serve_with` (spec D6, D7, AC2, AC5): mismatched overrides
//! stop start-up before any datastore is contacted; `PUSH_AUTH_PROVIDERS=external` with
//! an authenticator signs a user in over a socket, with PostgreSQL and Redis (Docker must
//! be running).

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::ImageExt;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::entra_server::ready;
use super::{FakeIdentity, RequireGroup, fake_token};
use crate::identity::Authenticator;
use crate::{Config, Overrides, serve_with};

fn database_url(port: u16) -> String {
    format!("postgres://postgres:postgres@127.0.0.1:{}/postgres", port)
}

/// auth-svc's variables: `database` and `redis` URLs, `providers`.
fn config(database: String, redis: String, providers: &str) -> Config {
    let variables: Vec<(String, String)> = vec![
        ("PUSH_DATABASE_URL".to_owned(), database),
        ("PUSH_REDIS_URL".to_owned(), redis),
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
        ("PUSH_AUTH_PROVIDERS".to_owned(), providers.to_owned()),
    ];
    Config::from_lookup(|name| {
        variables
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap()
}

#[tokio::test]
async fn mismatched_overrides_stop_start_up_before_any_datastore() {
    let acme: Arc<dyn Authenticator> = FakeIdentity::new("acme");
    let cases = [
        ("external", Overrides::default(), "Overrides.authenticator"),
        (
            "password",
            Overrides {
                authenticator: Some(acme.clone()),
                admission: None,
            },
            "does not name external",
        ),
        (
            "password",
            Overrides {
                authenticator: None,
                admission: Some(RequireGroup::new("staff")),
            },
            "neither entra nor external",
        ),
    ];
    for (providers, overrides, expected) in cases {
        // Nothing listens on port 1: a datastore contacted would be retried for ever.
        let config = config(database_url(1), "redis://127.0.0.1:1".to_owned(), providers);
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let started = tokio::time::timeout(
            Duration::from_secs(10),
            serve_with(config, http, health, CancellationToken::new(), overrides),
        )
        .await
        .expect("start-up stops at once");
        let message = started.unwrap_err().to_string();
        assert!(message.contains(expected), "{providers}: {message}");
        assert!(message.contains("PUSH_AUTH_PROVIDERS"), "{message}");
    }
}

#[tokio::test]
async fn a_wrapper_with_its_own_authenticator_signs_users_in_over_a_socket() {
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
    let config = config(
        database_url(postgres.get_host_port_ipv4(5432).await.unwrap()),
        format!(
            "redis://127.0.0.1:{}",
            redis.get_host_port_ipv4(6379).await.unwrap()
        ),
        "external",
    );
    let acme = FakeIdentity::new("acme");
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (addr, health_addr) = (http.local_addr().unwrap(), health.local_addr().unwrap());
    let shutdown = CancellationToken::new();
    let overrides = Overrides {
        authenticator: Some(acme.clone()),
        admission: Some(RequireGroup::new("staff")),
    };
    let server = tokio::spawn(serve_with(
        config,
        http,
        health,
        shutdown.clone(),
        overrides,
    ));

    let client = reqwest::Client::new();
    ready(&client, health_addr).await;
    let token = fake_token("https://id.acme.test/", "s-1", "ann@acme.test", "staff");
    let signed_in = client
        .post(format!("http://{addr}/v1/auth/acme"))
        .bearer_auth(&token)
        .json(&json!({"handle": "ann"}))
        .send()
        .await
        .unwrap();
    assert_eq!(signed_in.status(), 200);
    let outsider = fake_token("https://id.acme.test/", "s-2", "", "");
    let refused = client
        .post(format!("http://{addr}/v1/auth/acme"))
        .bearer_auth(&outsider)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 403);
    for route in ["/v1/auth/login", "/v1/auth/entra"] {
        let answer = client
            .post(format!("http://{addr}{route}"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status(), 404, "{route}");
    }
    assert_eq!(acme.calls(), 2);

    shutdown.cancel();
    server.await.unwrap().unwrap();
}
