//! The Entra ID sign-in API through the router (spec AC1 to AC6): the routes of each
//! offer, the endpoint, refusals, the session's end, key rotation and outages, rate limits
//! and the security log.

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use super::{Offer, World, capture_logs, captured_logs, security_events, world};
use crate::entra::testing::{CLIENT_APP, KEY_A, KEY_B, TENANT, sign};
use crate::service::unix_now;

const USER: &str = "55555555-0000-4000-8000-000000000005";

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    json: Value,
}

/// One request from the client 198.51.100.7, as the ingress reports it.
async fn call(
    world: &World,
    method: Method,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-real-ip", "198.51.100.7");
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        None => request.body(Body::empty()),
        Some(json) => request
            .header("content-type", "application/json")
            .body(Body::from(json.to_string())),
    }
    .unwrap();
    let response = world.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

async fn entra(world: &World, body: Value) -> Answer {
    call(world, Method::POST, "/v1/auth/entra", None, Some(body)).await
}

/// `object_id` signs in with Entra ID and registers a device: its tokens.
async fn with_device(world: &World, object_id: &str) -> Value {
    let signed_in = entra(world, json!({"token": world.stub.token(object_id)})).await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.json);
    let registration = signed_in.json["registration_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let device = call(
        world,
        Method::POST,
        "/v1/devices",
        Some(&registration),
        Some(json!({"name": "Laptop", "platform": "web", "registration_id": Uuid::now_v7()})),
    )
    .await;
    assert_eq!(device.status, StatusCode::CREATED, "{}", device.json);
    device.json
}

#[tokio::test]
async fn each_offer_routes_only_its_own_sign_in() {
    let entra_only = world(Offer::Entra).await;
    for (method, uri) in [
        (Method::POST, "/v1/auth/signup"),
        (Method::GET, "/v1/auth/verify?token=x"),
        (Method::POST, "/v1/auth/verify"),
        (Method::POST, "/v1/auth/verify/resend"),
        (Method::POST, "/v1/auth/login"),
        (Method::POST, "/v1/auth/password/forgot"),
        (Method::GET, "/v1/auth/password/reset?token=x"),
        (Method::POST, "/v1/auth/password/reset"),
    ] {
        let answer = call(&entra_only, method.clone(), uri, None, Some(json!({}))).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
    for (method, uri) in [
        (Method::POST, "/v1/auth/entra"),
        (Method::POST, "/v1/auth/refresh"),
        (Method::GET, "/v1/auth/jwks"),
        (Method::GET, "/v1/users/me"),
    ] {
        let answer = call(&entra_only, method.clone(), uri, None, Some(json!({}))).await;
        assert_ne!(answer.status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
    // The default: email and password only, no Entra ID endpoint (AC1).
    let password_only = world(Offer::Password).await;
    let token = password_only.stub.token(USER);
    assert_eq!(
        entra(&password_only, json!({"token": token})).await.status,
        StatusCode::NOT_FOUND
    );
    let login = call(
        &password_only,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({})),
    )
    .await;
    assert_ne!(login.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_entra_user_signs_in_registers_and_refreshes_and_no_email_is_written() {
    let world = world(Offer::Entra).await;
    let signed_in = entra(
        &world,
        json!({"token": world.stub.token(USER), "handle": "erin"}),
    )
    .await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.json);
    assert_eq!(signed_in.json["expires_in_s"], 300);
    let registration = signed_in.json["registration_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let device = call(
        &world,
        Method::POST,
        "/v1/devices",
        Some(&registration),
        Some(json!({"name": "Laptop", "platform": "web", "registration_id": Uuid::now_v7()})),
    )
    .await;
    assert_eq!(device.status, StatusCode::CREATED, "{}", device.json);
    assert!(device.json["refresh_expires_in_s"].as_i64().unwrap() <= 24 * 3600);
    let refreshed = call(
        &world,
        Method::POST,
        "/v1/auth/refresh",
        None,
        Some(json!({"refresh_token": device.json["refresh_token"]})),
    )
    .await;
    assert_eq!(refreshed.status, StatusCode::OK);
    let me = call(
        &world,
        Method::GET,
        "/v1/users/me",
        refreshed.json["access_token"].as_str(),
        None,
    )
    .await;
    assert_eq!(me.json["handle"], "erin");
    assert_eq!(me.json["display_name"], "Test User");
    assert_eq!(me.json["email"], "test.user@contoso.test");
    let kinds: Vec<String> = world
        .outbox()
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(kinds, ["user.verified", "device.registered"]);
}

#[tokio::test]
async fn refused_tokens_answer_one_code_and_create_nobody() {
    let world = world(Offer::Entra).await;
    let good = world.stub.user_claims(TENANT, USER);
    let mut id_token_audience = good.clone();
    id_token_audience["aud"] = json!(CLIENT_APP);
    let mut expired = good.clone();
    expired["exp"] = json!(unix_now() - 120);
    let mut app_only = good.clone();
    app_only.as_object_mut().unwrap().remove("scp");
    for token in [
        sign(KEY_A, &id_token_audience),
        sign(KEY_A, &expired),
        sign(KEY_B, &good),
        sign(KEY_A, &app_only),
        "garbage".to_owned(),
    ] {
        let answer = entra(&world, json!({"token": token})).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
        assert_eq!(answer.json["error"]["code"], "entra_token_invalid");
        assert_eq!(answer.json["error"]["retryable"], false);
        assert_eq!(answer.headers["www-authenticate"], "Bearer");
    }
    assert_eq!(world.accounts().await, 0);
    let no_token = entra(&world, json!({"handle": "erin"})).await;
    assert_eq!(no_token.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_session_ends_at_its_maximum_age_and_an_entra_sign_in_restores_the_device() {
    let world = world(Offer::Entra).await;
    let tokens = with_device(&world, USER).await;
    let device_id = tokens["device_id"].as_str().unwrap().to_owned();
    sqlx::query("UPDATE auth.device SET session_expires_at = $1")
        .bind(unix_now() - 1)
        .execute(&world.pool)
        .await
        .unwrap();
    let ended = call(
        &world,
        Method::POST,
        "/v1/auth/refresh",
        None,
        Some(json!({"refresh_token": tokens["refresh_token"]})),
    )
    .await;
    assert_eq!(ended.status, StatusCode::UNAUTHORIZED);
    assert_eq!(ended.json["error"]["code"], "session_expired");

    let again = entra(
        &world,
        json!({"token": world.stub.token(USER), "device_id": device_id}),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.json);
    assert_eq!(again.json["device_id"], device_id.as_str());
    let refreshed = call(
        &world,
        Method::POST,
        "/v1/auth/refresh",
        None,
        Some(json!({"refresh_token": again.json["refresh_token"]})),
    )
    .await;
    assert_eq!(refreshed.status, StatusCode::OK);
    let stranger = entra(
        &world,
        json!({"token": world.stub.token(USER), "device_id": Uuid::now_v7()}),
    )
    .await;
    assert_eq!(stranger.status, StatusCode::NOT_FOUND);
    assert_eq!(stranger.json["error"]["code"], "device_not_found");
}

#[tokio::test]
async fn an_outage_fails_closed_and_a_rotated_key_works_without_a_restart() {
    let world = world(Offer::Entra).await;
    world.stub.set_failing(true);
    let down = entra(&world, json!({"token": world.stub.token(USER)})).await;
    assert_eq!(down.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(down.json["error"]["code"], "entra_unavailable");
    assert_eq!(down.json["error"]["retryable"], true);
    assert_eq!(down.headers["retry-after"], "5");
    world.stub.set_failing(false);
    let up = entra(&world, json!({"token": world.stub.token(USER)})).await;
    assert_eq!(up.status, StatusCode::OK);

    world.stub.publish(&[KEY_B]);
    let rotated = sign(KEY_B, &world.stub.user_claims(TENANT, USER));
    assert_eq!(
        entra(&world, json!({"token": rotated})).await.status,
        StatusCode::OK
    );
    assert_eq!(world.accounts().await, 1);
}

#[tokio::test]
async fn sign_ins_are_rate_limited_per_ip_before_any_token_is_checked() {
    let world = world(Offer::Entra).await;
    for _ in 0..60 {
        let refused = entra(&world, json!({"token": "garbage"})).await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    }
    let limited = entra(&world, json!({"token": world.stub.token(USER)})).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.headers.contains_key("retry-after"));
    assert_eq!(
        world.stub.metadata_fetches(),
        0,
        "nothing was fetched for them"
    );
}

/// The capture is the process's global subscriber (a thread's own misses events while
/// other tests run in parallel), so the test looks for its own user's events.
#[tokio::test]
async fn entra_sign_ins_are_logged_without_tokens_object_ids_names_or_addresses() {
    capture_logs();
    let world = world(Offer::Entra).await;
    let accepted = world.stub.token(USER);
    assert_eq!(
        entra(&world, json!({"token": accepted})).await.status,
        StatusCode::OK
    );
    let mut claims = world.stub.user_claims(TENANT, USER);
    claims["aud"] = json!(CLIENT_APP);
    let refused = sign(KEY_A, &claims);
    assert_eq!(
        entra(&world, json!({"token": refused})).await.status,
        StatusCode::UNAUTHORIZED
    );
    let user: Uuid = sqlx::query_scalar("SELECT user_id FROM auth.external_identity")
        .fetch_one(&world.pool)
        .await
        .unwrap();

    let user = user.to_string();
    let events = security_events();
    let has = |name: &str, field: &str, value: &str| {
        events
            .iter()
            .any(|event| event["event"] == name && event[field] == value)
    };
    let mine = |name: &str| {
        events
            .iter()
            .find(|event| event["event"] == name && event["user_id"] == user.as_str())
            .cloned()
            .unwrap_or_else(|| panic!("no {name} of {user} in {events:?}"))
    };
    assert_eq!(mine("auth.entra.provisioned")["tenant"], TENANT);
    assert_eq!(mine("auth.entra.success")["ip"], "198.51.100.7");
    assert!(has("auth.entra.rejected", "reason", "audience"));
    assert!(has("auth.entra.failure", "code", "entra_token_invalid"));
    let text = captured_logs();
    for secret in [
        accepted.as_str(),
        refused.as_str(),
        USER,
        "Test User",
        "test.user@contoso.test",
    ] {
        assert!(!text.contains(secret), "the log holds {secret}");
    }
}
