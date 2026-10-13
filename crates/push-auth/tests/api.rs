//! The HTTP API end to end over a real PostgreSQL (Docker must be running): requests go
//! through the router as the ingress would send them, with the client IP in `x-real-ip`.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, Method, Request, StatusCode};
use http_body_util::BodyExt;
use push_auth::api::{Api, router};
use push_auth::limits::{Limits, MemoryCounter};
use push_auth::links::LinkSigner;
use push_auth::password::Passwords;
use push_auth::tokens::AccessTokens;
use push_auth::{Auth, Settings, migrate_database};
use serde_json::{Value, json};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tower::ServiceExt;
use uuid::Uuid;

struct World {
    _container: ContainerAsync<Postgres>,
    router: Router,
}

async fn world(dev_mailbox: bool) -> World {
    let container = Postgres::default()
        .with_tag("17-alpine")
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    // Docker publishes ports on IPv4; "localhost" would try IPv6 first on every connect.
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    migrate_database(&url).await.expect("migrate");
    let pool = PgPool::connect(&url).await.expect("connect");
    let auth = Auth::new(
        pool,
        Settings {
            public_url: "http://push.local".to_owned(),
            passwords: Passwords::new(64, 1, 1).unwrap(),
            links: LinkSigner::new(vec![3; 32]).unwrap(),
            tokens: AccessTokens::from_seed(&[5; 32], "ws://push.local/v1/socket").unwrap(),
            hash_concurrency: 4,
            hash_queue: 64,
            hash_wait: Duration::from_secs(5),
        },
    );
    let router = router(Api {
        auth,
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: Some(HeaderName::from_static("x-real-ip")),
        dev_mailbox,
    });
    World {
        _container: container,
        router,
    }
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    text: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|_| panic!("not JSON: {}", self.text))
    }
}

/// One request from the client 198.51.100.7; `body` is sent as JSON unless it is a
/// string, which is sent as a form.
async fn call(
    world: &World,
    method: Method,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> Answer {
    call_from(world, "198.51.100.7", method, uri, bearer, body).await
}

/// One request from the client `ip`, as the ingress reports it in `x-real-ip`.
async fn call_from(
    world: &World,
    ip: &str,
    method: Method,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-real-ip", ip);
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        None => request.body(Body::empty()),
        Some(Value::String(form)) => request
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(form)),
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
        text: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

/// Signs up `name@example.com` and returns the token of its verification link, read from
/// the DEV ONLY mailbox.
async fn sign_up(world: &World, name: &str) -> String {
    let answer = call(
        world,
        Method::POST,
        "/v1/auth/signup",
        None,
        Some(json!({
            "email": format!("{name}@example.com"),
            "password": "correct horse",
            "handle": name,
            "display_name": name,
        })),
    )
    .await;
    assert_eq!(answer.status, StatusCode::ACCEPTED, "{}", answer.text);
    assert_eq!(answer.json(), json!({"status": "check_email"}));
    let mailbox = call(
        world,
        Method::GET,
        &format!("/dev/mailbox?to={name}@example.com"),
        None,
        None,
    )
    .await
    .json();
    let link = mailbox["emails"][0]["params"]["link"].as_str().unwrap();
    assert!(
        link.starts_with("http://push.local/v1/auth/verify?token="),
        "{link}"
    );
    link.split_once("?token=").unwrap().1.to_owned()
}

/// A verified account with one registered device: its tokens.
async fn with_device(world: &World, name: &str) -> Value {
    let token = sign_up(world, name).await;
    let verified = call(
        world,
        Method::POST,
        "/v1/auth/verify",
        None,
        Some(json!({"token": token})),
    )
    .await;
    assert_eq!(verified.json(), json!({"status": "verified"}));
    let login = call(
        world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": format!("{name}@example.com"), "password": "correct horse"})),
    )
    .await;
    assert_eq!(login.status, StatusCode::OK, "{}", login.text);
    let registration = login.json()["registration_token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(login.json()["expires_in_s"], 300);
    let device = call(
        world,
        Method::POST,
        "/v1/devices",
        Some(&registration),
        Some(json!({
            "name": "Phone",
            "platform": "ios",
            "registration_id": Uuid::now_v7(),
        })),
    )
    .await;
    assert_eq!(device.status, StatusCode::CREATED, "{}", device.text);
    device.json()
}

#[tokio::test]
async fn sign_up_verify_register_and_use_a_device() {
    let world = world(true).await;
    let tokens = with_device(&world, "alice").await;
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in_s"], 900);
    let access = tokens["access_token"].as_str().unwrap();

    let devices = call(&world, Method::GET, "/v1/devices", Some(access), None).await;
    assert_eq!(devices.status, StatusCode::OK);
    assert_eq!(devices.headers["cache-control"], "no-store");
    let listed = &devices.json()["devices"][0];
    assert_eq!(listed["device_id"], tokens["device_id"]);
    assert_eq!(listed["platform"], "ios");
    assert_eq!(listed["current"], true);

    let me = call(&world, Method::GET, "/v1/users/me", Some(access), None).await;
    assert_eq!(me.json()["email"], "alice@example.com");
    assert_eq!(me.json()["handle"], "alice");
    let renamed = call(
        &world,
        Method::PATCH,
        "/v1/users/me",
        Some(access),
        Some(json!({"display_name": "Alice A."})),
    )
    .await;
    assert_eq!(renamed.json()["display_name"], "Alice A.");
    let found = call(
        &world,
        Method::GET,
        "/v1/users/lookup?handle=%40Alice",
        Some(access),
        None,
    )
    .await;
    assert_eq!(
        found.json(),
        json!({"user_id": tokens["user_id"], "handle": "alice", "display_name": "Alice A."})
    );
    assert!(found.json().get("email").is_none());

    let refreshed = call(
        &world,
        Method::POST,
        "/v1/auth/refresh",
        None,
        Some(json!({"refresh_token": tokens["refresh_token"]})),
    )
    .await;
    assert_eq!(refreshed.status, StatusCode::OK, "{}", refreshed.text);
    let again = call(
        &world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({
            "email": "alice@example.com",
            "password": "correct horse",
            "device_id": tokens["device_id"],
        })),
    )
    .await;
    assert_eq!(again.json()["device_id"], tokens["device_id"]);
    let access = again.json()["access_token"].as_str().unwrap().to_owned();

    let revoked = call(
        &world,
        Method::DELETE,
        &format!("/v1/devices/{}", tokens["device_id"].as_str().unwrap()),
        Some(&access),
        None,
    )
    .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    let after = call(&world, Method::GET, "/v1/devices", Some(&access), None).await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    assert_eq!(after.json()["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn link_pages_change_nothing_and_leak_nothing() {
    let world = world(true).await;
    let token = sign_up(&world, "alice").await;
    let page = call(
        &world,
        Method::GET,
        &format!("/v1/auth/verify?token={token}"),
        None,
        None,
    )
    .await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.headers["referrer-policy"], "no-referrer");
    assert_eq!(page.headers["cache-control"], "no-store");
    assert!(
        page.headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'none'")
    );
    assert!(
        page.text.contains(&format!(r#"value="{token}""#)),
        "{}",
        page.text
    );
    let hostile = call(
        &world,
        Method::GET,
        "/v1/auth/verify?token=%22%3E%3Cscript%3E",
        None,
        None,
    )
    .await;
    assert!(!hostile.text.contains("<script>"), "{}", hostile.text);

    // Opening the page did not verify: login still says so.
    let login = call(
        &world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": "alice@example.com", "password": "correct horse"})),
    )
    .await;
    assert_eq!(login.status, StatusCode::FORBIDDEN);
    assert_eq!(login.json()["error"]["code"], "email_not_verified");

    // The page's button posts a form and gets a page back.
    let done = call(
        &world,
        Method::POST,
        "/v1/auth/verify",
        None,
        Some(Value::String(format!("token={token}"))),
    )
    .await;
    assert_eq!(done.status, StatusCode::OK);
    assert!(
        done.text.contains("Your email address is verified"),
        "{}",
        done.text
    );
    let bad = call(
        &world,
        Method::POST,
        "/v1/auth/verify",
        None,
        Some(Value::String("token=forged.token".to_owned())),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(bad.text.contains("not valid"), "{}", bad.text);
}

#[tokio::test]
async fn a_password_reset_works_through_its_page() {
    let world = world(true).await;
    with_device(&world, "alice").await;
    let forgot = call(
        &world,
        Method::POST,
        "/v1/auth/password/forgot",
        None,
        Some(json!({"email": "alice@example.com"})),
    )
    .await;
    assert_eq!(forgot.status, StatusCode::ACCEPTED);
    let mailbox = call(
        &world,
        Method::GET,
        "/dev/mailbox?to=ALICE@example.com",
        None,
        None,
    )
    .await
    .json();
    assert_eq!(mailbox["emails"][0]["template"], "reset_password");
    let link = mailbox["emails"][0]["params"]["link"].as_str().unwrap();
    let path = link.strip_prefix("http://push.local").unwrap();
    let page = call(&world, Method::GET, path, None, None).await;
    assert_eq!(page.headers["referrer-policy"], "no-referrer");
    assert!(page.text.contains(r#"type="password""#), "{}", page.text);

    let token = link.split_once("?token=").unwrap().1;
    let changed = call(
        &world,
        Method::POST,
        "/v1/auth/password/reset",
        None,
        Some(Value::String(format!(
            "token={token}&password=battery+staple"
        ))),
    )
    .await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.text);
    let login = call(
        &world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": "alice@example.com", "password": "battery staple"})),
    )
    .await;
    assert_eq!(login.status, StatusCode::OK, "{}", login.text);
}

#[tokio::test]
async fn errors_have_one_format() {
    let world = world(true).await;
    let garbage = call(
        &world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": 1})),
    )
    .await;
    assert_eq!(garbage.status, StatusCode::BAD_REQUEST);
    assert_eq!(garbage.json()["error"]["code"], "invalid");
    assert_eq!(garbage.json()["error"]["retryable"], false);

    let wrong = call(
        &world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": "nobody@example.com", "password": "correct horse"})),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.json()["error"]["code"], "invalid_credentials");

    let anonymous = call(&world, Method::GET, "/v1/users/me", None, None).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous.headers["www-authenticate"], "Bearer");

    let jwks = call(&world, Method::GET, "/v1/auth/jwks", None, None).await;
    assert_eq!(jwks.json()["keys"][0]["crv"], "Ed25519");
    assert_eq!(jwks.headers["cache-control"], "public, max-age=300");
}

#[tokio::test]
async fn login_is_rate_limited_per_account_and_ip_without_locking_the_account() {
    let world = world(true).await;
    with_device(&world, "alice").await;
    // with_device logged in once; 9 more wrong attempts fit in the window.
    for _ in 0..9 {
        let answer = call(
            &world,
            Method::POST,
            "/v1/auth/login",
            None,
            Some(json!({"email": "alice@example.com", "password": "wrong horse"})),
        )
        .await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    }
    let limited = call(
        &world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": "alice@example.com", "password": "correct horse"})),
    )
    .await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.json()["error"]["code"], "rate_limited");
    assert_eq!(limited.json()["error"]["retryable"], true);
    let wait: u64 = limited.headers["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=900).contains(&wait), "{wait}");

    // The owner signs in from another address.
    let elsewhere = call_from(
        &world,
        "203.0.113.9",
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": "alice@example.com", "password": "correct horse"})),
    )
    .await;
    assert_eq!(elsewhere.status, StatusCode::OK, "{}", elsewhere.text);
}

#[tokio::test]
async fn the_dev_mailbox_exists_only_when_enabled() {
    let world = world(false).await;
    let answer = call(
        &world,
        Method::GET,
        "/dev/mailbox?to=a@example.com",
        None,
        None,
    )
    .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
}
