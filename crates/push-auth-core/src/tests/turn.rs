//! `GET /v1/calls/ice-servers` over HTTP (spec 2026-11-03 voice calls §8).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderName, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use super::{Offer, World, capture_logs, captured_logs, password_user, world};
use crate::api::{Api, router};
use crate::directory::NoDirectory;
use crate::limits::{Limits, MemoryCounter};
use crate::templates::NoTemplates;
use crate::turn::{Secret, Source, Turn, TurnSettings, rest_credential};

const SECRET: &str = "a-shared-secret-of-the-test-coturn";

fn public(world: &World, turn: Turn) -> Router {
    router(Api {
        auth: world.auth.clone(),
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: Some(HeaderName::from_static("x-real-ip")),
        directory: Arc::new(NoDirectory),
        templates: Arc::new(NoTemplates),
        call_history: Arc::new(crate::calls::NoCalls),
        agent_owners: Arc::new(crate::owners::NoAgentOwners),
        turn: Arc::new(turn),
    })
}

fn internal() -> Turn {
    Turn::new(Some(TurnSettings {
        ttl_seconds: 600,
        stun_urls: vec!["stun:turn.push.local:3478".to_owned()],
        source: Source::Rest {
            internal: true,
            urls: vec!["turn:turn.push.local:3478?transport=udp".to_owned()],
            secret: Secret::new(SECRET),
        },
    }))
    .unwrap()
}

async fn ice_servers(router: &Router, bearer: Option<&str>) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .uri("/v1/calls/ice-servers")
        .header("x-real-ip", "198.51.100.7");
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let mut request = request.body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 9], 40000))));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn a_signed_in_user_gets_ice_servers_for_themselves() {
    capture_logs();
    let world = world(Offer::Password).await;
    let tokens = password_user(&world.auth, &world.pool, "turnuser", 1).await;
    let router = public(&world, internal());

    let (status, headers, body) = ice_servers(&router, Some(&tokens[0].access_token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(body["relay"], true);
    let expires_at = body["expires_at"].as_str().unwrap();
    let expires = chrono::DateTime::parse_from_rfc3339(expires_at).unwrap();
    let ttl = expires.timestamp() - chrono::Utc::now().timestamp();
    assert!((595..=600).contains(&ttl), "{ttl}");
    let servers = body["ice_servers"].as_array().unwrap();
    assert_eq!(
        servers[0],
        serde_json::json!({"urls": ["stun:turn.push.local:3478"]})
    );
    let username = servers[1]["username"].as_str().unwrap();
    assert_eq!(
        username,
        format!("{}:{}", expires.timestamp(), tokens[0].user_id)
    );
    let credential = servers[1]["credential"].as_str().unwrap();
    assert_eq!(credential, rest_credential(SECRET, username));

    let logs = captured_logs();
    assert!(!logs.contains(credential), "the credential is logged");
    assert!(!logs.contains(SECRET), "the secret is logged");
}

#[tokio::test]
async fn ice_servers_need_an_access_token_and_report_unavailable_without_turn() {
    let world = world(Offer::Password).await;
    let tokens = password_user(&world.auth, &world.pool, "noturn", 1).await;

    let (status, headers, _) = ice_servers(&public(&world, internal()), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers["www-authenticate"], "Bearer");

    let (status, headers, body) = ice_servers(
        &public(&world, Turn::disabled()),
        Some(&tokens[0].access_token),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers["retry-after"], "30");
    assert_eq!(body["error"]["code"], "turn_unavailable");
    assert_eq!(body["error"]["retryable"], true);
}

#[tokio::test]
async fn ice_servers_are_limited_to_60_an_hour_per_user() {
    let world = world(Offer::Password).await;
    let tokens = password_user(&world.auth, &world.pool, "manyturns", 1).await;
    let router = public(&world, internal());
    for _ in 0..60 {
        let (status, _, _) = ice_servers(&router, Some(&tokens[0].access_token)).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, headers, body) = ice_servers(&router, Some(&tokens[0].access_token)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"]["code"], "rate_limited");
    assert!(headers.contains_key("retry-after"));
}
