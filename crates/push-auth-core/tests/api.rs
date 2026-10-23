//! The HTTP API end to end over a real PostgreSQL (Docker must be running): requests go
//! through the router as the ingress would send them, with the client IP in `x-real-ip`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, Method, Request, StatusCode};
use http_body_util::BodyExt;
use push_auth_core::api::{Api, router};
use push_auth_core::limits::{Limits, MemoryCounter};
use push_auth_core::links::LinkSigner;
use push_auth_core::password::Passwords;
use push_auth_core::tokens::AccessTokens;
use push_auth_core::{Auth, AuthError, Directory, Settings, migrate_database};
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
    pool: PgPool,
    directory: Arc<Relations>,
}

/// message-svc's `Directory` in miniature: who is related to whom, and whether it answers.
#[derive(Debug, Default)]
struct Relations {
    pairs: Mutex<Vec<(Uuid, Uuid)>>,
    down: AtomicBool,
    /// Ids the answer contains although nobody asked for them.
    extra: Mutex<Vec<Uuid>>,
}

impl Relations {
    /// `a` and `b` share a contact or a conversation.
    fn relate(&self, a: Uuid, b: Uuid) {
        self.pairs.lock().unwrap().extend([(a, b), (b, a)]);
    }
}

#[async_trait]
impl Directory for Relations {
    async fn visible_users(
        &self,
        caller: Uuid,
        candidates: &[Uuid],
    ) -> Result<Vec<Uuid>, AuthError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(AuthError::Unavailable);
        }
        let pairs = self.pairs.lock().unwrap();
        let mut visible: Vec<Uuid> = candidates
            .iter()
            .copied()
            .filter(|user| *user == caller || pairs.contains(&(caller, *user)))
            .collect();
        visible.extend(self.extra.lock().unwrap().iter().copied());
        Ok(visible)
    }
}

async fn world() -> World {
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
        pool.clone(),
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
    let directory = Arc::new(Relations::default());
    let router = router(Api {
        auth,
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: Some(HeaderName::from_static("x-real-ip")),
        directory: Arc::clone(&directory) as Arc<dyn Directory>,
    });
    World {
        _container: container,
        router,
        pool,
        directory,
    }
}

/// The link of the newest `template` email to `to`, from the outbox, where the worker
/// would pick it up.
async fn last_link(world: &World, to: &str, template: &str) -> String {
    sqlx::query_scalar(
        "SELECT payload->'params'->>'link' FROM auth.outbox WHERE kind = 'email.send' \
         AND payload->>'to' = $1 AND payload->>'template' = $2 \
         ORDER BY created_at DESC, event_id DESC LIMIT 1",
    )
    .bind(to)
    .bind(template)
    .fetch_one(&world.pool)
    .await
    .unwrap()
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
/// the outbox.
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
    let link = last_link(world, &format!("{name}@example.com"), "verify_email").await;
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
    let world = world().await;
    let tokens = with_device(&world, "alice").await;
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in_s"], 1800);
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
        json!({"user_id": tokens["user_id"], "handle": "alice", "display_name": "Alice A.", "kind":"user", "owner_user_id":null})
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
    let world = world().await;
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
    let world = world().await;
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
    let link = last_link(&world, "alice@example.com", "reset_password").await;
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
    let world = world().await;
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

/// Security events are logged with the client IP and the account's ids, and never with an
/// email address, a password or a token. The capture is the process's global subscriber:
/// a thread's own subscriber misses events while other tests run in parallel.
#[tokio::test]
async fn security_events_are_logged_without_secrets() {
    static LINES: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(|| Capture(&LINES))
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let world = world().await;
    let tokens = with_device(&world, "audited").await;
    let refresh = tokens["refresh_token"].as_str().unwrap();
    let wrong = call(
        &world,
        Method::POST,
        "/v1/auth/login",
        None,
        Some(json!({"email": "audited@example.com", "password": "wrong horse"})),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    let refused = call(
        &world,
        Method::POST,
        "/v1/auth/refresh",
        None,
        Some(json!({"refresh_token": "not-a-refresh-token"})),
    )
    .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);

    let text = String::from_utf8(LINES.lock().unwrap().clone()).unwrap();
    let events: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| line["target"] == "security")
        .map(|line| line["fields"].clone())
        .collect();
    let named = |name: &str| {
        events
            .iter()
            .find(|event| event["event"] == name)
            .unwrap_or_else(|| panic!("no {name} in {events:?}"))
            .clone()
    };
    let registered = events
        .iter()
        .find(|event| {
            event["event"] == "auth.device.registered" && event["user_id"] == tokens["user_id"]
        })
        .expect("the registration is logged");
    assert_eq!(registered["device_id"], tokens["device_id"]);
    assert_eq!(registered["ip"], "198.51.100.7");
    assert_eq!(named("auth.email.verified")["ip"], "198.51.100.7");
    assert_eq!(named("auth.login.success")["status"], 200);
    let failure = events
        .iter()
        .find(|event| {
            event["event"] == "auth.login.failure" && event["code"] == "invalid_credentials"
        })
        .expect("the failed sign-in is logged");
    assert_eq!(failure["status"], 401);
    assert_eq!(failure["ip"], "198.51.100.7");
    assert_eq!(named("auth.refresh.refused")["route"], "/v1/auth/refresh");
    for secret in [
        "audited@example.com",
        "correct horse",
        "wrong horse",
        refresh,
        tokens["access_token"].as_str().unwrap(),
    ] {
        assert!(!text.contains(secret), "the log holds {secret}");
    }
}

struct Capture(&'static Mutex<Vec<u8>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn login_is_rate_limited_per_account_and_ip_without_locking_the_account() {
    let world = world().await;
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
async fn profiles_are_shown_only_to_related_users() {
    let world = world().await;
    let alice = with_device(&world, "alice").await;
    let bob = with_device(&world, "bob").await;
    let carol = with_device(&world, "carol").await;
    let id = |tokens: &Value| Uuid::parse_str(tokens["user_id"].as_str().unwrap()).unwrap();
    world.directory.relate(id(&alice), id(&bob));
    let access = alice["access_token"].as_str().unwrap();
    let ask = |ids: &[Uuid]| {
        let ids: Vec<String> = ids.iter().map(Uuid::to_string).collect();
        format!("/v1/users?ids={}", ids.join(","))
    };

    // Carol is not related to Alice, and an unknown id is left out like her.
    let unknown = Uuid::now_v7();
    let uri = ask(&[id(&carol), id(&bob), unknown, id(&alice)]);
    let answer = call(&world, Method::GET, &uri, Some(access), None).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text);
    assert_eq!(answer.headers["cache-control"], "no-store");
    let mut expected = vec![
        json!({"user_id": alice["user_id"], "handle": "alice", "display_name": "alice", "kind":"user", "owner_user_id":null}),
        json!({"user_id": bob["user_id"], "handle": "bob", "display_name": "bob", "kind":"user", "owner_user_id":null}),
    ];
    expected.sort_by_key(|user| user["user_id"].as_str().unwrap().to_owned());
    assert_eq!(answer.json(), json!({ "users": expected }));

    let answer = call(&world, Method::GET, "/v1/users?ids=x", Some(access), None).await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert_eq!(answer.json()["error"]["code"], "invalid");
    let answer = call(&world, Method::GET, &ask(&[id(&bob)]), None, None).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);

    // Without message-svc's answer no profile is shown.
    world.directory.down.store(true, Ordering::SeqCst);
    let answer = call(&world, Method::GET, &ask(&[id(&bob)]), Some(access), None).await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(answer.json()["error"]["retryable"], true);
}

#[tokio::test]
async fn a_profile_nobody_asked_for_is_never_returned() {
    let world = world().await;
    let alice = with_device(&world, "alice").await;
    let bob = with_device(&world, "bob").await;
    let carol = with_device(&world, "carol").await;
    let id = |tokens: &Value| Uuid::parse_str(tokens["user_id"].as_str().unwrap()).unwrap();
    world.directory.relate(id(&alice), id(&bob));
    // message-svc's answer wrongly contains Carol, whom Alice did not ask for.
    world.directory.extra.lock().unwrap().push(id(&carol));
    let access = alice["access_token"].as_str().unwrap();

    let uri = format!("/v1/users?ids={}", id(&bob));
    let answer = call(&world, Method::GET, &uri, Some(access), None).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text);
    let users = answer.json()["users"].as_array().unwrap().clone();
    assert_eq!(users.len(), 1, "{}", answer.text);
    assert_eq!(users[0]["handle"], "bob");
    assert!(!answer.text.contains("carol"), "{}", answer.text);
}

async fn patch_me(world: &World, token: &str, body: Value) -> Answer {
    call(
        world,
        Method::PATCH,
        "/v1/users/me",
        Some(token),
        Some(body),
    )
    .await
}

async fn lookup_status(world: &World, token: &str, handle: &str) -> StatusCode {
    let uri = format!("/v1/users/lookup?handle={handle}");
    call(world, Method::GET, &uri, Some(token), None)
        .await
        .status
}

#[tokio::test]
async fn a_user_changes_their_handle_and_the_old_one_is_free_at_once() {
    let world = world().await;
    let alice = with_device(&world, "alice").await;
    let bob = with_device(&world, "bob").await;
    let alice_access = alice["access_token"].as_str().unwrap();
    let bob_access = bob["access_token"].as_str().unwrap();

    let renamed = patch_me(&world, alice_access, json!({"handle": "@Alice_2"})).await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.text);
    assert_eq!(renamed.json()["handle"], "alice_2");
    assert_eq!(renamed.json()["display_name"], "alice");
    assert_eq!(
        lookup_status(&world, bob_access, "alice").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        lookup_status(&world, bob_access, "alice_2").await,
        StatusCode::OK
    );

    let taken = patch_me(&world, bob_access, json!({"handle": "alice_2"})).await;
    assert_eq!(taken.status, StatusCode::CONFLICT);
    assert_eq!(taken.json()["error"]["code"], "handle_taken");
    let freed = patch_me(
        &world,
        bob_access,
        json!({"handle": "alice", "display_name": "Bob B."}),
    )
    .await;
    assert_eq!(freed.status, StatusCode::OK, "{}", freed.text);
    assert_eq!(freed.json()["handle"], "alice");
    assert_eq!(freed.json()["display_name"], "Bob B.");

    for bad in [
        json!({}),
        json!({"handle": "x"}),
        json!({"handle": "9lives"}),
        json!({"display_name": ""}),
    ] {
        let answer = patch_me(&world, alice_access, bad.clone()).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    // A display name alone is changed as before and is not counted as a handle change.
    let named = patch_me(&world, alice_access, json!({"display_name": "Alice A."})).await;
    assert_eq!(named.json()["handle"], "alice_2");
    // 5 handle changes a day per account: Alice made 3 (the two invalid ones count).
    for handle in ["alice_3", "alice_4"] {
        let answer = patch_me(&world, alice_access, json!({"handle": handle})).await;
        assert_eq!(answer.status, StatusCode::OK, "{handle}");
    }
    let limited = patch_me(&world, alice_access, json!({"handle": "alice_5"})).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.headers.contains_key("retry-after"));
    let unauthenticated = call(
        &world,
        Method::PATCH,
        "/v1/users/me",
        None,
        Some(json!({"handle": "carol"})),
    )
    .await;
    assert_eq!(unauthenticated.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_hyphenated_handle_signs_up_and_is_found_ignoring_case() {
    let world = world().await;
    let yury = with_device(&world, "yury-opolev").await;
    let access = yury["access_token"].as_str().unwrap();
    let found = call(
        &world,
        Method::GET,
        "/v1/users/lookup?handle=%40Yury-Opolev",
        Some(access),
        None,
    )
    .await;
    assert_eq!(found.status, StatusCode::OK, "{}", found.text);
    assert_eq!(found.json()["user_id"], yury["user_id"]);
    assert_eq!(found.json()["handle"], "yury-opolev");

    let renamed = patch_me(&world, access, json!({"handle": "yury-o_1"})).await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.text);
    assert_eq!(renamed.json()["handle"], "yury-o_1");
    for bad in ["yury-", "yury--o", "-yury"] {
        let answer = patch_me(&world, access, json!({"handle": bad})).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{bad}");
    }
}
