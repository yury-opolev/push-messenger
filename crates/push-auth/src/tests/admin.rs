//! The admin API (spec AC10): each selector, refusals, the rate limit, the relay events
//! and the security log, through the admin router.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use super::{
    Offer, World, capture_logs, captured_logs, entra_user, password_user, security_events, world,
};
use crate::admin::{AdminApi, admin_router};
use crate::entra::testing::TENANT;
use crate::error::AuthError;
use crate::limits::{Limits, MemoryCounter};

const ADMIN_TOKEN: &str = "admin-token-of-the-auth-svc-tests-0123456789";

fn admin(world: &World) -> Router {
    admin_router(AdminApi::new(
        world.auth.clone(),
        Limits::new(Arc::new(MemoryCounter::default())),
        ADMIN_TOKEN,
    ))
}

async fn revoke(router: &Router, bearer: Option<&str>, body: Value) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/v1/admin/users/revoke-sessions")
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let mut request = request.body(Body::from(body.to_string())).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 9], 40000))));
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn an_operator_signs_a_user_out_of_every_device_by_handle_id_or_entra_identity() {
    capture_logs();

    let world = world(Offer::Both).await;
    let router = admin(&world);
    let alice = password_user(&world.auth, &world.pool, "alice", 2).await;
    let erin_oid = Uuid::now_v7();
    let erin = entra_user(&world.auth, erin_oid).await;

    let (status, answer) = revoke(&router, Some(ADMIN_TOKEN), json!({"handle": "@Alice"})).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer,
        json!({"user_id": alice[0].user_id, "devices_revoked": 2})
    );
    for tokens in &alice {
        assert_eq!(
            world.auth.refresh(&tokens.refresh_token).await,
            Err(AuthError::Unauthorized)
        );
        assert_eq!(
            world.auth.authenticate(&tokens.access_token).await,
            Err(AuthError::Unauthorized)
        );
    }
    let relayed: Vec<Value> = world
        .outbox()
        .await
        .into_iter()
        .filter(|(kind, _)| kind == "device.revoked")
        .map(|(_, payload)| payload["device_id"].clone())
        .collect();
    assert_eq!(
        relayed,
        alice
            .iter()
            .map(|tokens| json!(tokens.device_id))
            .collect::<Vec<_>>()
    );
    // Nothing is left to revoke; the account stays and may sign in again.
    let (status, again) = revoke(
        &router,
        Some(ADMIN_TOKEN),
        json!({"user_id": alice[0].user_id}),
    )
    .await;
    assert_eq!(
        (status, again["devices_revoked"].clone()),
        (StatusCode::OK, json!(0))
    );
    assert!(
        world
            .auth
            .login("alice@example.com", "correct horse")
            .await
            .is_ok()
    );

    let (status, answer) = revoke(
        &router,
        Some(ADMIN_TOKEN),
        json!({"entra": {"tenant_id": TENANT, "object_id": erin_oid}}),
    )
    .await;
    assert_eq!(
        (status, answer["devices_revoked"].clone()),
        (StatusCode::OK, json!(1))
    );
    assert_eq!(
        world.auth.refresh(&erin.refresh_token).await,
        Err(AuthError::Unauthorized)
    );

    let alice_id = alice[0].user_id.to_string();
    let event = security_events()
        .into_iter()
        .find(|event| {
            event["event"] == "auth.admin.sessions_revoked" && event["user_id"] == alice_id.as_str()
        })
        .expect("the revocation is a security event");
    assert_eq!(event["selector"], "handle");
    assert_eq!(event["devices"], 2);
    assert!(!captured_logs().contains(ADMIN_TOKEN));
}

#[tokio::test]
async fn the_admin_api_refuses_wrong_tokens_and_bodies_and_is_rate_limited() {
    let world = world(Offer::Both).await;
    let router = admin(&world);
    password_user(&world.auth, &world.pool, "alice", 1).await;
    let short = &ADMIN_TOKEN[1..];
    for bearer in [
        None,
        Some("wrong-token-wrong-token-wrong-token-wrong"),
        Some(short),
    ] {
        let (status, answer) = revoke(&router, bearer, json!({"handle": "alice"})).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bearer:?}");
        assert_eq!(answer["error"]["code"], "unauthorized");
    }
    for body in [
        json!({}),
        json!({"user_id": Uuid::now_v7(), "handle": "alice"}),
        json!({"handle": "x"}),
        json!({"user": "alice"}),
    ] {
        let (status, _) = revoke(&router, Some(ADMIN_TOKEN), body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    for body in [
        json!({"user_id": Uuid::now_v7()}),
        json!({"handle": "nobody"}),
    ] {
        let (status, answer) = revoke(&router, Some(ADMIN_TOKEN), body).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(answer["error"]["code"], "user_not_found");
    }
    // Failed authentications lock the IP out of further guesses (20 per 15 minutes, 3 so
    // far), but never the holder of the token: they are counted on their own.
    for _ in 0..17 {
        let (status, _) = revoke(
            &router,
            Some("wrong-token-wrong-token-wrong-token-wrong"),
            json!({"handle": "nobody"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let (status, answer) = revoke(&router, None, json!({"handle": "nobody"})).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(answer["error"]["code"], "rate_limited");
    let (status, _) = revoke(&router, Some(ADMIN_TOKEN), json!({"handle": "nobody"})).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the token holder is not locked out"
    );
    // Authenticated calls: 60 per 15 minutes per IP, 7 so far.
    for _ in 0..53 {
        let (status, _) = revoke(&router, Some(ADMIN_TOKEN), json!({"handle": "nobody"})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let (status, _) = revoke(&router, Some(ADMIN_TOKEN), json!({"handle": "alice"})).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}
