//! The identity middleware through the router (spec D5, D9, AC2 to AC4, AC7): the token
//! from the header or the body, the rate limit before the authenticator, external sign-in,
//! the admission, and the security log.

use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use super::{
    FakeIdentity, Offer, RequireGroup, World, capture_logs, captured_logs, fake_token,
    security_events, world_with,
};
use crate::entra::testing::{KEY_A, TENANT, sign};

const ISSUER: &str = "https://id.acme.test/";
const USER: &str = "55555555-0000-4000-8000-000000000005";

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    json: Value,
}

/// One request from the client 198.51.100.7: `authorization` header values as given, a
/// JSON body unless `content_type` says otherwise.
async fn send(
    world: &World,
    method: Method,
    uri: &str,
    authorization: &[String],
    content_type: &str,
    body: String,
) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-real-ip", "198.51.100.7")
        .header("content-type", content_type);
    for value in authorization {
        request = request.header("authorization", value);
    }
    let response = world
        .router
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

/// `POST uri` with `Authorization: Bearer {token}` and the JSON `body`.
async fn with_header(world: &World, uri: &str, token: &str, body: Value) -> Answer {
    let bearer = format!("Bearer {token}");
    send(
        world,
        Method::POST,
        uri,
        &[bearer],
        "application/json",
        body.to_string(),
    )
    .await
}

/// `POST uri` with the JSON `body` only.
async fn with_body(world: &World, uri: &str, body: Value) -> Answer {
    send(
        world,
        Method::POST,
        uri,
        &[],
        "application/json",
        body.to_string(),
    )
    .await
}

/// Registers a device with `registration`: its tokens.
async fn register(world: &World, registration: &Value) -> Value {
    let bearer = format!("Bearer {}", registration.as_str().unwrap());
    let device = send(
        world,
        Method::POST,
        "/v1/devices",
        &[bearer],
        "application/json",
        json!({"name": "Laptop", "platform": "web", "registration_id": Uuid::now_v7()}).to_string(),
    )
    .await;
    assert_eq!(device.status, StatusCode::CREATED, "{}", device.json);
    device.json
}

#[tokio::test]
async fn the_token_comes_from_the_header_or_the_body_and_conflicts_are_refused() {
    let entra = world_with(Offer::Entra, None, None).await;
    let token = entra.stub.token(USER);
    for answer in [
        with_header(&entra, "/v1/auth/entra", &token, json!({})).await,
        with_body(&entra, "/v1/auth/entra", json!({"token": token})).await,
        with_header(&entra, "/v1/auth/entra", &token, json!({"token": token})).await,
    ] {
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.json);
        assert!(answer.json["registration_token"].is_string());
    }
    assert_eq!(entra.accounts().await, 1);

    let acme = FakeIdentity::new("acme");
    let world = world_with(Offer::Password, Some(acme.clone()), None).await;
    let good = fake_token(ISSUER, "s-1", "", "");
    let other = fake_token(ISSUER, "s-2", "", "");
    let too_long = "x".repeat(16 * 1024 + 1);
    let cases: Vec<(&str, Vec<String>, &str, String)> = vec![
        (
            "different tokens",
            vec![format!("Bearer {good}")],
            "application/json",
            json!({"token": other}).to_string(),
        ),
        (
            "not a bearer token",
            vec![format!("Basic {good}")],
            "application/json",
            "{}".to_owned(),
        ),
        (
            "an empty bearer token",
            vec!["Bearer ".to_owned()],
            "application/json",
            "{}".to_owned(),
        ),
        (
            "two headers",
            vec![format!("Bearer {good}"), format!("Bearer {good}")],
            "application/json",
            "{}".to_owned(),
        ),
        (
            "a header token over 16 KiB",
            vec![format!("Bearer {too_long}")],
            "application/json",
            "{}".to_owned(),
        ),
        (
            "no token",
            vec![],
            "application/json",
            json!({"handle": "ann"}).to_string(),
        ),
        (
            "a token that is not a string",
            vec![],
            "application/json",
            json!({"token": 7}).to_string(),
        ),
        (
            "a body that is not JSON",
            vec![format!("Bearer {good}")],
            "application/json",
            "token".to_owned(),
        ),
        (
            "a body that is not an object",
            vec![format!("Bearer {good}")],
            "application/json",
            json!([good]).to_string(),
        ),
        (
            "not a JSON request",
            vec![format!("Bearer {good}")],
            "text/plain",
            "{}".to_owned(),
        ),
        (
            "an empty body",
            vec![format!("Bearer {good}")],
            "application/json",
            String::new(),
        ),
    ];
    for (case, authorization, content_type, body) in cases {
        let answer = send(
            &world,
            Method::POST,
            "/v1/auth/acme",
            &authorization,
            content_type,
            body,
        )
        .await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{case}");
        assert_eq!(answer.json["error"]["code"], "invalid", "{case}");
    }
    assert_eq!(acme.calls(), 0, "the authenticator was never asked");
    assert_eq!(world.accounts().await, 0);
}

#[tokio::test]
async fn sign_ins_are_rate_limited_before_the_authenticator_is_asked() {
    let acme = FakeIdentity::new("acme");
    let world = world_with(Offer::Password, Some(acme.clone()), None).await;
    for _ in 0..60 {
        let refused = with_header(&world, "/v1/auth/acme", "bad", json!({})).await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
        assert_eq!(refused.json["error"]["code"], "token_invalid");
    }
    let good = fake_token(ISSUER, "s-1", "", "");
    let limited = with_header(&world, "/v1/auth/acme", &good, json!({})).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.headers.contains_key("retry-after"));
    assert_eq!(acme.calls(), 60);
}

#[tokio::test]
async fn an_external_provider_signs_in_registers_and_renews() {
    let world = world_with(Offer::Password, Some(FakeIdentity::new("acme")), None).await;
    let token = fake_token(ISSUER, "s-1", "ann@acme.test", "");
    let signed_in = with_header(&world, "/v1/auth/acme", &token, json!({"handle": "ann"})).await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.json);
    assert_eq!(signed_in.json["expires_in_s"], 300);
    let tokens = register(&world, &signed_in.json["registration_token"]).await;
    assert!(tokens["refresh_expires_in_s"].as_i64().unwrap() <= 2 * 3600);

    // The same identity with another address: the same account and device.
    let renamed = fake_token(ISSUER, "s-1", "ann.other@acme.test", "");
    let renewed = with_body(
        &world,
        "/v1/auth/acme",
        json!({"token": renamed, "device_id": tokens["device_id"]}),
    )
    .await;
    assert_eq!(renewed.status, StatusCode::OK, "{}", renewed.json);
    assert_eq!(renewed.json["device_id"], tokens["device_id"]);
    assert_eq!(world.accounts().await, 1);
    let me = send(
        &world,
        Method::GET,
        "/v1/users/me",
        &[format!(
            "Bearer {}",
            renewed.json["access_token"].as_str().unwrap()
        )],
        "application/json",
        String::new(),
    )
    .await;
    assert_eq!(me.json["handle"], "ann");
    assert_eq!(me.json["email"], "ann.other@acme.test");

    let refused = with_header(&world, "/v1/auth/acme", "bad", json!({})).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert_eq!(refused.json["error"]["code"], "token_invalid");
    assert_eq!(refused.json["error"]["retryable"], false);
    assert_eq!(refused.headers["www-authenticate"], "Bearer");
    let foreign = token.replacen("ok|", "foreign|", 1);
    let refused = with_header(&world, "/v1/auth/acme", &foreign, json!({})).await;
    assert_eq!(refused.json["error"]["code"], "token_invalid");
    let down = with_header(&world, "/v1/auth/acme", "down", json!({})).await;
    assert_eq!(down.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(down.json["error"]["code"], "identity_unavailable");
    assert_eq!(down.json["error"]["retryable"], true);
    assert_eq!(down.headers["retry-after"], "5");
    let entra = with_body(&world, "/v1/auth/entra", json!({"token": token})).await;
    assert_eq!(entra.status, StatusCode::NOT_FOUND);
    assert_eq!(world.accounts().await, 1);
}

#[tokio::test]
async fn an_admission_refusal_creates_nothing_and_its_outage_is_retryable() {
    let staff = RequireGroup::new("staff");
    let world = world_with(
        Offer::Password,
        Some(FakeIdentity::new("acme")),
        Some(staff.clone()),
    )
    .await;
    let outsider = fake_token(ISSUER, "s-1", "", "guests");
    let refused = with_header(&world, "/v1/auth/acme", &outsider, json!({})).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.json);
    assert_eq!(refused.json["error"]["code"], "sign_in_not_permitted");
    assert_eq!(refused.json["error"]["retryable"], false);
    assert_eq!(world.accounts().await, 0);

    let member = fake_token(ISSUER, "s-1", "", "guests,staff");
    let admitted = with_header(&world, "/v1/auth/acme", &member, json!({})).await;
    assert_eq!(admitted.status, StatusCode::OK, "{}", admitted.json);
    let tokens = register(&world, &admitted.json["registration_token"]).await;
    // Removed from the group: the device's renewal is refused and changes nothing.
    let renewal = with_header(
        &world,
        "/v1/auth/acme",
        &outsider,
        json!({"device_id": tokens["device_id"]}),
    )
    .await;
    assert_eq!(renewal.status, StatusCode::FORBIDDEN);
    assert_eq!(renewal.json["error"]["code"], "sign_in_not_permitted");
    let refreshed = with_body(
        &world,
        "/v1/auth/refresh",
        json!({"refresh_token": tokens["refresh_token"]}),
    )
    .await;
    assert_eq!(
        refreshed.status,
        StatusCode::OK,
        "no renewal revoked the refresh token"
    );
    assert_eq!(world.accounts().await, 1);

    staff.down.store(true, Ordering::SeqCst);
    let down = with_header(&world, "/v1/auth/acme", &member, json!({})).await;
    assert_eq!(down.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(down.json["error"]["code"], "identity_unavailable");
    assert_eq!(down.json["error"]["retryable"], true);
    let seen = staff.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[1]["groups"], json!(["guests", "staff"]));

    // Entra ID sign-in passes the same admission, with the token's claims.
    let staff = RequireGroup::new("staff");
    let entra = world_with(Offer::Entra, None, Some(staff.clone())).await;
    let mut claims = entra.stub.user_claims(TENANT, USER);
    let no_group = sign(KEY_A, &claims);
    claims["groups"] = json!(["staff"]);
    let in_group = sign(KEY_A, &claims);
    let refused = with_body(&entra, "/v1/auth/entra", json!({"token": no_group})).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(entra.accounts().await, 0);
    let admitted = with_body(&entra, "/v1/auth/entra", json!({"token": in_group})).await;
    assert_eq!(admitted.status, StatusCode::OK);
    assert_eq!(staff.seen.lock().unwrap()[1]["tid"], TENANT);
}

/// The capture is the process's global subscriber, so the test looks for its own values.
#[tokio::test]
async fn external_sign_ins_are_logged_without_tokens_subjects_names_or_addresses() {
    capture_logs();
    let unique = Uuid::now_v7().simple().to_string();
    let issuer = format!("https://{unique}.acme.test/");
    let subject = format!("subject-{unique}");
    let email = format!("{unique}@acme.test");
    let world = world_with(
        Offer::Password,
        Some(FakeIdentity::new("acme")),
        Some(RequireGroup::new("staff")),
    )
    .await;
    let member = fake_token(&issuer, &subject, &email, "staff");
    let outsider = fake_token(&issuer, &subject, &email, "");
    let signed_in = with_header(&world, "/v1/auth/acme", &member, json!({})).await;
    assert_eq!(signed_in.status, StatusCode::OK);
    let refused = with_header(&world, "/v1/auth/acme", &outsider, json!({})).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let rejected = format!("bad-{unique}");
    assert_eq!(
        with_header(&world, "/v1/auth/acme", &rejected, json!({}))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let user: Uuid = sqlx::query_scalar("SELECT user_id FROM auth.external_identity")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    let user = user.to_string();

    let events = security_events();
    let find = |name: &str, field: &str, value: &str| {
        events
            .iter()
            .find(|event| event["event"] == name && event[field] == value)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} with {field} {value} in {events:?}"))
    };
    let provisioned = find("auth.external.provisioned", "user_id", &user);
    assert_eq!(provisioned["provider"], "acme");
    assert_eq!(provisioned["issuer"], issuer.as_str());
    assert_eq!(
        find("auth.external.success", "user_id", &user)["route"],
        "/v1/auth/acme"
    );
    assert_eq!(
        find("auth.external.not_permitted", "issuer", &issuer)["provider"],
        "acme"
    );
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "auth.external.rejected"
                && event["provider"] == "acme"
                && event["reason"] == "signature")
    );
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "auth.external.failure"
                && event["code"] == "sign_in_not_permitted")
    );
    let text = captured_logs();
    for secret in [
        member.as_str(),
        outsider.as_str(),
        rejected.as_str(),
        subject.as_str(),
        email.as_str(),
        "Acme User",
    ] {
        assert!(!text.contains(secret), "the log holds {secret}");
    }
}
