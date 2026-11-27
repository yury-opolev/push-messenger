//! The Entra ID sign-in API through the router (spec AC1 to AC6): the routes of each
//! offer, the endpoint, refusals, the session's end, key rotation and outages, rate limits
//! and the security log.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use super::{Offer, World, capture_logs, captured_logs, security_events, world};
use crate::entra::testing::{
    CLIENT_APP, KEY_A, KEY_B, StubValidator, TENANT, ValidatorAnswer, sign,
};
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

fn offer_app(world: &mut World) {
    let app = world
        .auth
        .entra()
        .unwrap()
        .app(crate::entra::EntraAppSettings {
            max_session: std::time::Duration::from_secs(3600),
            ..Default::default()
        })
        .unwrap();
    world.auth = world.auth.clone().with_entra_app(app);
    rebuild_router(world, None);
}

/// Routes `world.auth` again, with fresh rate limits.
fn rebuild_router(world: &mut World, client_ip_header: Option<HeaderName>) {
    world.router = crate::api::router(crate::api::Api {
        auth: world.auth.clone(),
        limits: crate::limits::Limits::new(std::sync::Arc::new(
            crate::limits::MemoryCounter::default(),
        )),
        client_ip_header,
        directory: std::sync::Arc::new(crate::directory::NoDirectory),
        templates: std::sync::Arc::new(crate::templates::NoTemplates),
        call_history: std::sync::Arc::new(crate::calls::NoCalls),
        agent_owners: std::sync::Arc::new(crate::owners::NoAgentOwners),
        turn: std::sync::Arc::new(crate::turn::Turn::disabled()),
    });
}

fn app_token(world: &World) -> String {
    let mut claims = world.stub.user_claims(TENANT, USER);
    claims.as_object_mut().unwrap().remove("scp");
    claims["idtyp"] = json!("app");
    sign(KEY_A, &claims)
}

#[tokio::test]
async fn app_sign_in_never_provisions_an_unlinked_identity() {
    capture_logs();
    let mut world = world(Offer::Entra).await;
    let disabled = call(
        &world,
        Method::POST,
        "/v1/auth/entra-app",
        None,
        Some(json!({"token": app_token(&world)})),
    )
    .await;
    assert_eq!(disabled.status, StatusCode::NOT_FOUND);
    offer_app(&mut world);
    let principal = world
        .auth
        .authenticator("entra-app")
        .unwrap()
        .authenticate(&app_token(&world))
        .await
        .unwrap();
    assert!(matches!(
        world.auth.sign_in_external(&principal, Some("agent")).await,
        Err(crate::error::AuthError::SignInNotPermitted)
    ));
    for body in [
        json!({"token": app_token(&world), "handle": "agent"}),
        json!({"token": app_token(&world), "device_id": Uuid::new_v4()}),
    ] {
        let response = call(&world, Method::POST, "/v1/auth/entra-app", None, Some(body)).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        assert_eq!(response.json["error"]["code"], "sign_in_not_permitted");
    }
    assert_eq!(world.accounts().await, 0);
    assert!(world.outbox().await.is_empty());
    let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM auth.external_identity")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(identities, 0);
    assert!(!captured_logs().contains(&app_token(&world)));
    assert!(
        security_events()
            .iter()
            .any(|event| event["event"] == "auth.external.failure"
                && event["route"] == "/v1/auth/entra-app")
    );
}

#[tokio::test]
async fn app_route_rejects_bad_crypto_before_admission_or_account_lookup() {
    let admission = super::RequireGroup::new("agents");
    let mut world = super::world_with(Offer::Entra, None, Some(admission.clone())).await;
    offer_app(&mut world);
    let mut good = world.stub.user_claims(TENANT, USER);
    good.as_object_mut().unwrap().remove("scp");
    good["idtyp"] = json!("app");
    for (claim, value) in [
        ("aud", json!(CLIENT_APP)),
        ("exp", json!(unix_now() - 120)),
        ("nbf", json!(unix_now() + 120)),
        ("iss", json!("https://evil.example/v2.0")),
        ("ver", json!("1.0")),
        ("oid", json!("not-a-guid")),
        ("idtyp", json!("user")),
        ("scp", json!("access_as_user")),
    ] {
        let mut bad = good.clone();
        bad[claim] = value;
        let response = call(
            &world,
            Method::POST,
            "/v1/auth/entra-app",
            None,
            Some(json!({"token":sign(KEY_A, &bad)})),
        )
        .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{claim}");
        assert_eq!(response.json["error"]["code"], "token_invalid", "{claim}");
    }
    for bad in [
        crate::entra::testing::sign_as(KEY_B, KEY_A.kid, &good),
        crate::entra::testing::with_algorithm("HS256", KEY_A.kid, &good, "fake"),
    ] {
        assert_eq!(
            call(
                &world,
                Method::POST,
                "/v1/auth/entra-app",
                None,
                Some(json!({"token":bad}))
            )
            .await
            .status,
            StatusCode::UNAUTHORIZED
        );
    }
    assert!(admission.seen.lock().unwrap().is_empty());
    let refused = call(
        &world,
        Method::POST,
        "/v1/auth/entra-app",
        None,
        Some(json!({"token":app_token(&world)})),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(admission.seen.lock().unwrap()[0]["azp"], CLIENT_APP);
    admission
        .down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let unavailable = call(
        &world,
        Method::POST,
        "/v1/auth/entra-app",
        None,
        Some(json!({"token":app_token(&world)})),
    )
    .await;
    assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(world.accounts().await, 0);
    assert!(world.outbox().await.is_empty());
}

#[tokio::test]
async fn linked_app_sign_in_reuses_registration_and_device_sessions_with_its_own_policy() {
    let mut world = world(Offer::Entra).await;
    offer_app(&mut world);
    // A delegated identity with the identical tid/oid must not link the app.
    let delegated = entra(&world, json!({"token": world.stub.token(USER)})).await;
    assert_eq!(delegated.status, StatusCode::OK);
    let unlinked = call(
        &world,
        Method::POST,
        "/v1/auth/entra-app",
        None,
        Some(json!({"token": app_token(&world)})),
    )
    .await;
    assert_eq!(unlinked.status, StatusCode::FORBIDDEN);
    let user = Uuid::now_v7();
    sqlx::query("INSERT INTO auth.user_account (user_id, handle, display_name, email_verified_at) VALUES ($1, 'agent', 'Agent', now())")
        .bind(user).execute(&world.pool).await.unwrap();
    // Fixture for linked-only sign-in; production owner consent is tested in agents.
    sqlx::query("INSERT INTO auth.external_identity (identity_id, provider, issuer, subject, user_id) VALUES ($1, 'entra-app', $2, $3, $4)")
        .bind(Uuid::now_v7()).bind(TENANT).bind(USER).bind(user).execute(&world.pool).await.unwrap();
    let linked = call(
        &world,
        Method::POST,
        "/v1/auth/entra-app",
        None,
        Some(json!({"token": app_token(&world)})),
    )
    .await;
    assert_eq!(linked.status, StatusCode::OK, "{}", linked.json);
    let device = call(
        &world,
        Method::POST,
        "/v1/devices",
        linked.json["registration_token"].as_str(),
        Some(json!({"name":"Agent", "platform":"web", "registration_id":Uuid::now_v7()})),
    )
    .await;
    assert_eq!(device.status, StatusCode::CREATED, "{}", device.json);
    assert!(device.json["refresh_expires_in_s"].as_i64().unwrap() <= 3600);
    let renewed = call(
        &world,
        Method::POST,
        "/v1/auth/entra-app",
        None,
        Some(json!({"token":app_token(&world), "device_id":device.json["device_id"]})),
    )
    .await;
    assert_eq!(renewed.status, StatusCode::OK);
    assert!(renewed.json["refresh_expires_in_s"].as_i64().unwrap() <= 3600);
    let delegated_to_app = call(
        &world,
        Method::POST,
        "/v1/auth/entra-app",
        None,
        Some(json!({"token":world.stub.token(USER)})),
    )
    .await;
    assert_eq!(delegated_to_app.status, StatusCode::UNAUTHORIZED);
    assert_eq!(delegated_to_app.json["error"]["code"], "token_invalid");
    assert_eq!(
        entra(&world, json!({"token":app_token(&world)}))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(world.accounts().await, 2);
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

#[tokio::test]
async fn an_entra_user_changes_their_handle_with_an_access_token_alone() {
    capture_logs();
    let world = world(Offer::Entra).await;
    let tokens = with_device(&world, USER).await;
    let renamed = call(
        &world,
        Method::PATCH,
        "/v1/users/me",
        tokens["access_token"].as_str(),
        Some(json!({"handle": "erin_renamed"})),
    )
    .await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.json);
    assert_eq!(renamed.json["handle"], "erin_renamed");
    // A later sign-in keeps the chosen handle, whatever it asks for.
    let again = entra(
        &world,
        json!({"token": world.stub.token(USER), "handle": "something_else"}),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK);
    let handle: String = sqlx::query_scalar("SELECT handle::text FROM auth.user_account")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(handle, "erin_renamed");

    assert!(
        security_events().iter().any(|event| {
            event["event"] == "auth.handle.changed" && event["user_id"] == tokens["user_id"]
        }),
        "the change is a security event"
    );
    assert!(
        !captured_logs().contains("erin_renamed"),
        "the handle is not logged"
    );
}

/// Switches `world` to an external token validator for Entra ID (and `entra-app` with
/// `app`); the router keeps the client IP in `x-real-ip`.
async fn validator_mode(world: &mut World, app: bool) -> StubValidator {
    let validator = StubValidator::start().await;
    let settings = validator.settings_for(&world.stub);
    use_validator(world, settings, app);
    validator
}

/// As [`validator_mode`] for Entra ID alone, with the header protocol.
async fn header_validator_mode(world: &mut World) -> StubValidator {
    let validator = StubValidator::start().await;
    let settings = validator.settings_for_headers(&world.stub);
    use_validator(world, settings, false);
    validator
}

fn use_validator(world: &mut World, settings: crate::entra::EntraSettings, app: bool) {
    let entra = crate::entra::EntraSignIn::with_timing(settings, super::quick()).unwrap();
    assert!(entra.uses_validator());
    let mut auth = world.auth.clone().with_sign_in(crate::service::SignIn {
        password: false,
        entra: Some(entra.clone()),
    });
    if app {
        let app = entra
            .app(crate::entra::EntraAppSettings {
                max_session: std::time::Duration::from_secs(3600),
                ..Default::default()
            })
            .unwrap();
        auth = auth.with_entra_app(app);
    }
    world.auth = auth;
    rebuild_router(world, Some(HeaderName::from_static("x-real-ip")));
}

#[tokio::test]
async fn a_validator_checks_entra_sign_ins_without_any_key_fetch() {
    capture_logs();
    let mut world = world(Offer::Entra).await;
    let validator = validator_mode(&mut world, false).await;
    let token = world.stub.token(USER);
    let signed_in = entra(&world, json!({"token": token})).await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.json);
    assert_eq!(world.accounts().await, 1);
    assert_eq!(validator.requests().len(), 1);
    assert_eq!(world.stub.metadata_fetches(), 0);
    assert_eq!(world.stub.jwks_fetches(), 0);
    assert!(!captured_logs().contains(&token));
}

#[tokio::test]
async fn a_validator_refusal_is_logged_as_rejected_with_the_validator_flag() {
    capture_logs();
    let mut world = world(Offer::Entra).await;
    let validator = validator_mode(&mut world, false).await;
    validator.set_answer(ValidatorAnswer::Status(
        401,
        r#"{"error":"invalid_token","reason":"signature"}"#.to_owned(),
    ));
    let token = world.stub.token(USER);
    let refused = entra(&world, json!({"token": token})).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert_eq!(refused.json["error"]["code"], "entra_token_invalid");
    validator.set_answer(ValidatorAnswer::Status(401, String::new()));
    let bare = entra(&world, json!({"token": token})).await;
    assert_eq!(bare.status, StatusCode::UNAUTHORIZED);
    assert_eq!(bare.json["error"]["code"], "entra_token_invalid");
    assert_eq!(world.accounts().await, 0);
    let events = security_events();
    for reason in ["signature", "validator"] {
        assert!(
            events
                .iter()
                .any(|event| event["event"] == "auth.entra.rejected"
                    && event["reason"] == reason
                    && event["validator"] == true
                    && event["request_id"]
                        .as_str()
                        .and_then(|id| Uuid::try_parse(id).ok())
                        .is_some_and(|id| id.get_version_num() == 7)),
            "no validator refusal for {reason} with a request id in {events:?}"
        );
    }
    let text = captured_logs();
    assert!(!text.contains(&token), "the log holds the token");
    assert!(!text.contains(USER), "the log holds the object id");
}

#[tokio::test]
async fn an_unavailable_validator_fails_closed() {
    let mut world = world(Offer::Entra).await;
    let validator = validator_mode(&mut world, false).await;
    for answer in [
        ValidatorAnswer::Status(500, String::new()),
        ValidatorAnswer::Delay(std::time::Duration::from_millis(1500)),
    ] {
        validator.set_answer(answer.clone());
        let down = entra(&world, json!({"token": world.stub.token(USER)})).await;
        assert_eq!(down.status, StatusCode::SERVICE_UNAVAILABLE, "{answer:?}");
        assert_eq!(
            down.json["error"]["code"], "entra_unavailable",
            "{answer:?}"
        );
    }
    assert_eq!(world.accounts().await, 0);
}

#[tokio::test]
async fn an_app_route_refusal_in_validator_mode_carries_the_validator_flag() {
    capture_logs();
    let mut world = world(Offer::Entra).await;
    let validator = validator_mode(&mut world, true).await;
    let token = world.stub.token(USER);
    let refused = call(
        &world,
        Method::POST,
        "/v1/auth/entra-app",
        None,
        Some(json!({"token": token})),
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert_eq!(refused.json["error"]["code"], "token_invalid");
    assert_eq!(validator.requests().len(), 1);
    let events = security_events();
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "auth.external.rejected"
                && event["provider"] == "entra-app"
                && event["reason"] == "app_only"
                && event["validator"] == true),
        "no app-only validator refusal in {events:?}"
    );
    let text = captured_logs();
    assert!(!text.contains(&token), "the log holds the token");
    assert!(!text.contains(USER), "the log holds the object id");
}

#[tokio::test]
async fn built_in_refusals_carry_no_validator_flag() {
    capture_logs();
    let world = world(Offer::Entra).await;
    // An app-only token on the delegated route: no validator-mode test logs this
    // `auth.entra.rejected` reason, so every such event is a built-in refusal.
    let mut claims = world.stub.user_claims(TENANT, USER);
    claims["roles"] = json!(["Messages.Send"]);
    claims.as_object_mut().unwrap().remove("scp");
    let refused = sign(KEY_A, &claims);
    assert_eq!(
        entra(&world, json!({"token": refused})).await.status,
        StatusCode::UNAUTHORIZED
    );
    let events = security_events();
    let rejected: Vec<&Value> = events
        .iter()
        .filter(|event| event["event"] == "auth.entra.rejected")
        .collect();
    let app_only: Vec<&&Value> = rejected
        .iter()
        .filter(|event| event["reason"] == "app_only")
        .collect();
    assert!(!app_only.is_empty(), "{rejected:?}");
    assert!(
        app_only
            .iter()
            .all(|event| event.get("validator").is_none() && event.get("request_id").is_none()),
        "{app_only:?}"
    );
    assert!(!captured_logs().contains(&refused));
}

#[tokio::test]
async fn a_header_validator_hears_the_sign_in_request_without_its_query() {
    capture_logs();
    let mut world = world(Offer::Entra).await;
    let validator = header_validator_mode(&mut world).await;
    let token = world.stub.token(USER);
    let signed_in = call(
        &world,
        Method::POST,
        "/v1/auth/entra?x=1",
        None,
        Some(json!({"token": token})),
    )
    .await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.json);
    assert_eq!(world.accounts().await, 1);
    assert_eq!(
        (world.stub.metadata_fetches(), world.stub.jwks_fetches()),
        (0, 0)
    );
    let requests = validator.requests();
    assert_eq!(requests.len(), 1);
    let seen = &requests[0];
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.body, "");
    assert_eq!(seen.header("authorization"), [format!("Bearer {token}")]);
    assert_eq!(
        seen.header("original-uri"),
        ["http://push.local/v1/auth/entra"]
    );
    assert_eq!(seen.header("original-method"), ["POST"]);
    assert_eq!(seen.header("x-forwarded-for"), ["198.51.100.7"]);
    assert_eq!(seen.header("return-all-subject-token-claims"), ["1"]);
    let request_id = seen.header("x-request-id");
    assert_eq!(request_id.len(), 1);
    assert!(Uuid::try_parse(request_id[0]).is_ok(), "{request_id:?}");
    assert!(!captured_logs().contains(&token), "the log holds the token");
}

#[tokio::test]
async fn a_header_validator_refusal_leaks_nothing_and_names_the_request_id() {
    capture_logs();
    let mut world = world(Offer::Entra).await;
    let validator = header_validator_mode(&mut world).await;
    validator.set_answer(ValidatorAnswer::Headers {
        status: 401,
        headers: vec![(
            "WWW-Authenticate".to_owned(),
            r#"Bearer error="invalid_token", error_description="secret-desc""#.to_owned(),
        )],
    });
    let token = world.stub.token(USER);
    let refused = entra(&world, json!({"token": token})).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert_eq!(refused.json["error"]["code"], "entra_token_invalid");
    // push-auth's own challenge only, as for any refused token.
    let challenges: Vec<_> = refused.headers.get_all("www-authenticate").iter().collect();
    assert_eq!(challenges, ["Bearer"]);
    assert!(!refused.json.to_string().contains("secret-desc"));
    assert!(!refused.json.to_string().contains("invalid_token"));
    assert_eq!(world.accounts().await, 0);

    let requests = validator.requests();
    assert_eq!(requests.len(), 1);
    let request_id = requests[0].header("x-request-id")[0].to_owned();
    let events = security_events();
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "auth.entra.rejected"
                && event["reason"] == "validator"
                && event["validator"] == true
                && event["request_id"] == request_id.as_str()),
        "no refusal naming {request_id} in {events:?}"
    );
    let text = captured_logs();
    assert!(
        text.lines()
            .any(|line| line.contains("the token validator refused a token")
                && line.contains("invalid_token")),
        "the challenge's error is not logged"
    );
    assert!(!text.contains(&token), "the log holds the token");
    assert!(
        !text.contains("secret-desc"),
        "the log holds the description"
    );
    assert!(!text.contains(USER), "the log holds the object id");
}
