use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::json;
use uuid::Uuid;

use super::*;

const SECRET: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKL";
const CLOUDFLARE_TOKEN: &str = "cf-api-token-0123456789abcdef";
const TWILIO_TOKEN: &str = "twilio-auth-token-0123456789";
// Built from parts: a whole SID-shaped literal trips secret scanning.
const TWILIO_SID: &str = concat!("AC", "0123456789abcdef", "0123456789abcdef");
const PROVIDER_CREDENTIAL: &str = "provider-credential-0123456789";

fn settings(variables: &[(&str, &str)]) -> Result<Option<TurnSettings>, ConfigError> {
    let variables: HashMap<String, String> = variables
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    TurnSettings::from_lookup(&|name| variables.get(name).cloned())
}

fn internal() -> TurnSettings {
    settings(&[
        ("PUSH_TURN_MODE", "internal"),
        ("PUSH_TURN_STUN_URLS", "stun:turn.example.com:3478"),
        (
            "PUSH_TURN_URLS",
            "turn:turn.example.com:3478?transport=udp,turn:turn.example.com:3478?transport=tcp",
        ),
        ("PUSH_TURN_SHARED_SECRET", SECRET),
    ])
    .unwrap()
    .unwrap()
}

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).unwrap()
}

#[test]
fn the_rest_credential_matches_known_answers() {
    // Computed independently (Python hmac/hashlib/base64).
    assert_eq!(
        rest_credential("north", "1433895918:alice"),
        "RqMvcGPYTJMGThVLSi4amT4zFtI="
    );
    let user = Uuid::parse_str("01936b2e-1f3a-7c4d-8e5f-6a7b8c9d0e1f").unwrap();
    let username = rest_username(1_735_689_600, user);
    assert_eq!(username, "1735689600:01936b2e-1f3a-7c4d-8e5f-6a7b8c9d0e1f");
    assert_eq!(
        rest_credential(SECRET, &username),
        "VQi7KgKAXiAc6ZhbTP2coGjJMjw="
    );
}

#[tokio::test]
async fn credentials_expire_and_name_the_user() {
    let turn = Turn::new(Some(internal()))
        .unwrap()
        .with_clock(|| at(1_800_000_000) + TimeDelta::milliseconds(700));
    let user = Uuid::now_v7();
    let servers = turn.ice_servers(user).await.unwrap();
    assert!(servers.relay);
    assert_eq!(servers.expires_at, at(1_800_000_600));
    assert_eq!(servers.servers.len(), 2);
    assert_eq!(servers.servers[0].urls, ["stun:turn.example.com:3478"]);
    assert_eq!(servers.servers[0].username, None);
    let relay = &servers.servers[1];
    assert_eq!(relay.urls.len(), 2);
    let username = format!("1800000600:{user}");
    assert_eq!(relay.username.as_deref(), Some(username.as_str()));
    assert_eq!(
        relay.credential.as_deref(),
        Some(rest_credential(SECRET, &username).as_str())
    );
    // Another user, another credential.
    let other = turn.ice_servers(Uuid::now_v7()).await.unwrap();
    assert_ne!(other.servers[1].credential, relay.credential);
}

#[tokio::test]
async fn stun_only_answers_without_a_relay() {
    let settings = settings(&[
        ("PUSH_TURN_MODE", "stun-only"),
        ("PUSH_TURN_STUN_URLS", "stun:stun.l.google.com:19302"),
        ("PUSH_TURN_TTL_SECONDS", "60"),
    ])
    .unwrap()
    .unwrap();
    let turn = Turn::new(Some(settings))
        .unwrap()
        .with_clock(|| at(1_800_000_000));
    let servers = turn.ice_servers(Uuid::now_v7()).await.unwrap();
    assert!(!servers.relay);
    assert_eq!(servers.expires_at, at(1_800_000_060));
    assert_eq!(servers.servers.len(), 1);
    assert_eq!(servers.servers[0].credential, None);
}

#[tokio::test]
async fn without_settings_turn_is_unavailable() {
    let turn = Turn::disabled();
    assert_eq!(
        turn.ice_servers(Uuid::now_v7()).await,
        Err(AuthError::TurnUnavailable)
    );
    assert_eq!(turn.render_metrics(), "");
    assert_eq!(AuthError::TurnUnavailable.status(), 503);
    assert_eq!(AuthError::TurnUnavailable.code(), "turn_unavailable");
}

#[test]
fn settings_are_read_and_checked() {
    assert_eq!(settings(&[]), Ok(None));
    let internal = internal();
    assert_eq!(internal.ttl_seconds, 600);
    assert_eq!(internal.mode(), "internal");
    let generic = settings(&[
        ("PUSH_TURN_MODE", "external"),
        (
            "PUSH_TURN_URLS",
            "turns:relay.example.net:443?transport=tcp",
        ),
        ("PUSH_TURN_SHARED_SECRET", "a-provider-secret-value\n"),
        ("PUSH_TURN_TTL_SECONDS", "86400"),
    ])
    .unwrap()
    .unwrap();
    assert_eq!(generic.mode(), "generic");
    assert_eq!(
        generic.source,
        Source::Rest {
            internal: false,
            urls: vec!["turns:relay.example.net:443?transport=tcp".to_owned()],
            secret: Secret::new("a-provider-secret-value"),
        }
    );
    let cloudflare = settings(&[
        ("PUSH_TURN_MODE", "external"),
        ("PUSH_TURN_PROVIDER", "cloudflare"),
        ("PUSH_TURN_CLOUDFLARE_KEY_ID", "0123abcd"),
        ("PUSH_TURN_CLOUDFLARE_TOKEN", CLOUDFLARE_TOKEN),
    ])
    .unwrap()
    .unwrap();
    assert_eq!(cloudflare.mode(), "cloudflare");
    let twilio = settings(&[
        ("PUSH_TURN_MODE", "external"),
        ("PUSH_TURN_PROVIDER", "twilio"),
        ("PUSH_TURN_TWILIO_ACCOUNT_SID", TWILIO_SID),
        ("PUSH_TURN_TWILIO_TOKEN", TWILIO_TOKEN),
    ])
    .unwrap()
    .unwrap();
    assert_eq!(twilio.mode(), "twilio");

    let refused = |variables: &[(&str, &str)], expected: &str| {
        let error = settings(variables).unwrap_err();
        let named = match &error {
            ConfigError::Missing(name) => *name,
            ConfigError::Invalid { name, .. } => *name,
        };
        assert_eq!(named, expected, "{variables:?}: {error}");
        error
    };
    refused(&[("PUSH_TURN_MODE", "relay")], "PUSH_TURN_MODE");
    for ttl in ["59", "86401", "1.5", "ten"] {
        refused(
            &[
                ("PUSH_TURN_MODE", "stun-only"),
                ("PUSH_TURN_STUN_URLS", "stun:a"),
                ("PUSH_TURN_TTL_SECONDS", ttl),
            ],
            "PUSH_TURN_TTL_SECONDS",
        );
    }
    refused(&[("PUSH_TURN_MODE", "stun-only")], "PUSH_TURN_STUN_URLS");
    refused(
        &[
            ("PUSH_TURN_MODE", "stun-only"),
            ("PUSH_TURN_STUN_URLS", "turn:a:3478"),
        ],
        "PUSH_TURN_STUN_URLS",
    );
    refused(
        &[
            ("PUSH_TURN_MODE", "internal"),
            ("PUSH_TURN_SHARED_SECRET", SECRET),
        ],
        "PUSH_TURN_URLS",
    );
    refused(
        &[
            ("PUSH_TURN_MODE", "internal"),
            ("PUSH_TURN_URLS", "stun:a:3478"),
            ("PUSH_TURN_SHARED_SECRET", SECRET),
        ],
        "PUSH_TURN_URLS",
    );
    refused(
        &[("PUSH_TURN_MODE", "internal"), ("PUSH_TURN_URLS", "turn:a")],
        "PUSH_TURN_SHARED_SECRET",
    );
    let short = refused(
        &[
            ("PUSH_TURN_MODE", "internal"),
            ("PUSH_TURN_URLS", "turn:a"),
            ("PUSH_TURN_SHARED_SECRET", "short-secret"),
        ],
        "PUSH_TURN_SHARED_SECRET",
    );
    assert!(!short.to_string().contains("short-secret"), "{short}");
    refused(
        &[
            ("PUSH_TURN_MODE", "external"),
            ("PUSH_TURN_PROVIDER", "xirsys"),
        ],
        "PUSH_TURN_PROVIDER",
    );
    refused(
        &[
            ("PUSH_TURN_MODE", "external"),
            ("PUSH_TURN_PROVIDER", "cloudflare"),
            ("PUSH_TURN_CLOUDFLARE_KEY_ID", "not/an/id"),
            ("PUSH_TURN_CLOUDFLARE_TOKEN", CLOUDFLARE_TOKEN),
        ],
        "PUSH_TURN_CLOUDFLARE_KEY_ID",
    );
    refused(
        &[
            ("PUSH_TURN_MODE", "external"),
            ("PUSH_TURN_PROVIDER", "cloudflare"),
            ("PUSH_TURN_CLOUDFLARE_KEY_ID", "0123abcd"),
        ],
        "PUSH_TURN_CLOUDFLARE_TOKEN",
    );
    refused(
        &[
            ("PUSH_TURN_MODE", "external"),
            ("PUSH_TURN_PROVIDER", "twilio"),
            ("PUSH_TURN_TWILIO_ACCOUNT_SID", "AC123"),
            ("PUSH_TURN_TWILIO_TOKEN", TWILIO_TOKEN),
        ],
        "PUSH_TURN_TWILIO_ACCOUNT_SID",
    );
}

#[test]
fn provider_secrets_never_show_in_debug_output() {
    for variables in [
        vec![
            ("PUSH_TURN_MODE", "internal"),
            ("PUSH_TURN_URLS", "turn:a"),
            ("PUSH_TURN_SHARED_SECRET", SECRET),
        ],
        vec![
            ("PUSH_TURN_MODE", "external"),
            ("PUSH_TURN_PROVIDER", "cloudflare"),
            ("PUSH_TURN_CLOUDFLARE_KEY_ID", "0123abcd"),
            ("PUSH_TURN_CLOUDFLARE_TOKEN", CLOUDFLARE_TOKEN),
        ],
        vec![
            ("PUSH_TURN_MODE", "external"),
            ("PUSH_TURN_PROVIDER", "twilio"),
            ("PUSH_TURN_TWILIO_ACCOUNT_SID", TWILIO_SID),
            ("PUSH_TURN_TWILIO_TOKEN", TWILIO_TOKEN),
        ],
    ] {
        let shown = format!("{:?}", settings(&variables).unwrap());
        for secret in [SECRET, CLOUDFLARE_TOKEN, TWILIO_TOKEN] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }
    let server = IceServer {
        urls: vec!["turn:a".to_owned()],
        username: Some("1:user".to_owned()),
        credential: Some(PROVIDER_CREDENTIAL.to_owned()),
    };
    let shown = format!("{server:?}");
    assert!(!shown.contains(PROVIDER_CREDENTIAL) && !shown.contains("1:user"));
}

/// A stub of both providers' APIs: counts calls, checks the authentication and answers
/// `status`.
#[derive(Clone)]
struct Stub {
    calls: Arc<AtomicUsize>,
    status: Arc<Mutex<StatusCode>>,
    ttl_seen: Arc<Mutex<Option<String>>>,
}

impl Stub {
    async fn start() -> (Self, String) {
        let stub = Self {
            calls: Arc::default(),
            status: Arc::new(Mutex::new(StatusCode::CREATED)),
            ttl_seen: Arc::default(),
        };
        let app = Router::new()
            .route(
                "/v1/turn/keys/{key}/credentials/generate-ice-servers",
                post(cloudflare),
            )
            .route("/2010-04-01/Accounts/{sid}/Tokens.json", post(twilio))
            .with_state(stub.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (stub, url)
    }

    fn answer(&self) -> Option<StatusCode> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let status = *self.status.lock().unwrap();
        (!status.is_success()).then_some(status)
    }
}

async fn cloudflare(State(stub): State<Stub>, headers: HeaderMap, body: String) -> Response {
    assert_eq!(
        headers["authorization"],
        format!("Bearer {CLOUDFLARE_TOKEN}").as_str()
    );
    *stub.ttl_seen.lock().unwrap() = Some(body);
    if let Some(status) = stub.answer() {
        return (status, "{\"errors\":[]}").into_response();
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let call = stub.calls.load(Ordering::SeqCst);
    (
        StatusCode::CREATED,
        axum::Json(json!({"iceServers": [
            {"urls": ["stun:stun.cloudflare.com:3478", "stun:stun.cloudflare.com:53"]},
            {"urls": ["turn:turn.cloudflare.com:3478?transport=udp", "turns:turn.cloudflare.com:443?transport=tcp"],
             "username": format!("cf-user-{call}"), "credential": PROVIDER_CREDENTIAL}
        ]})),
    )
        .into_response()
}

async fn twilio(State(stub): State<Stub>, headers: HeaderMap, body: String) -> Response {
    let expected = format!(
        "Basic {}",
        STANDARD.encode(format!("{TWILIO_SID}:{TWILIO_TOKEN}"))
    );
    assert_eq!(headers["authorization"], expected.as_str());
    *stub.ttl_seen.lock().unwrap() = Some(body);
    if let Some(status) = stub.answer() {
        return (status, "{\"code\":20003}").into_response();
    }
    (
        StatusCode::CREATED,
        axum::Json(json!({
            "ttl": "300",
            "ice_servers": [
                {"urls": "stun:global.stun.twilio.com:3478", "url": "stun:global.stun.twilio.com:3478"},
                {"urls": "turn:global.turn.twilio.com:3478?transport=udp", "username": "tw-user", "credential": PROVIDER_CREDENTIAL},
                {"urls": "turn:global.turn.twilio.com:443?transport=tcp", "username": "tw-user", "credential": PROVIDER_CREDENTIAL}
            ]
        })),
    )
        .into_response()
}

fn cloudflare_settings() -> TurnSettings {
    settings(&[
        ("PUSH_TURN_MODE", "external"),
        ("PUSH_TURN_PROVIDER", "cloudflare"),
        ("PUSH_TURN_CLOUDFLARE_KEY_ID", "0123abcd"),
        ("PUSH_TURN_CLOUDFLARE_TOKEN", CLOUDFLARE_TOKEN),
        ("PUSH_TURN_STUN_URLS", "stun:extra.example.com:3478"),
    ])
    .unwrap()
    .unwrap()
}

/// A clock tests move by hand.
fn manual_clock(start: DateTime<Utc>) -> (Arc<Mutex<DateTime<Utc>>>, impl Fn() -> DateTime<Utc>) {
    let now = Arc::new(Mutex::new(start));
    let read = Arc::clone(&now);
    (now, move || *read.lock().unwrap())
}

#[tokio::test]
async fn cloudflare_credentials_are_minted_once_and_kept_per_user() {
    let (stub, url) = Stub::start().await;
    let (now, clock) = manual_clock(at(1_800_000_000));
    let turn = Arc::new(
        Turn::new(Some(cloudflare_settings()))
            .unwrap()
            .with_provider_base_url(&url)
            .unwrap()
            .with_clock(clock),
    );
    let user = Uuid::now_v7();
    // Two requests at once share one provider call.
    let (first, second) = tokio::join!(turn.ice_servers(user), turn.ice_servers(user));
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(stub.calls.load(Ordering::SeqCst), 1);
    assert_eq!(first, second);
    assert_eq!(
        stub.ttl_seen.lock().unwrap().as_deref(),
        Some(r#"{"ttl":600}"#)
    );
    assert!(first.relay);
    assert_eq!(first.expires_at, at(1_800_000_600));
    assert_eq!(first.servers[0].urls, ["stun:extra.example.com:3478"]);
    assert_eq!(first.servers[1].urls, ["stun:stun.cloudflare.com:3478"]);
    assert_eq!(first.servers[2].urls.len(), 2);
    assert_eq!(first.servers[2].username.as_deref(), Some("cf-user-1"));

    // Another user gets credentials of their own.
    turn.ice_servers(Uuid::now_v7()).await.unwrap();
    assert_eq!(stub.calls.load(Ordering::SeqCst), 2);

    // Kept until a minute before they expire, then minted again.
    *now.lock().unwrap() = at(1_800_000_539);
    let kept = turn.ice_servers(user).await.unwrap();
    assert_eq!(kept.expires_at, at(1_800_000_600));
    assert_eq!(stub.calls.load(Ordering::SeqCst), 2);
    *now.lock().unwrap() = at(1_800_000_540);
    let renewed = turn.ice_servers(user).await.unwrap();
    assert_eq!(stub.calls.load(Ordering::SeqCst), 3);
    assert_eq!(renewed.expires_at, at(1_800_001_140));
    assert_eq!(renewed.servers[2].username.as_deref(), Some("cf-user-3"));

    let metrics = turn.render_metrics();
    assert!(
        metrics.contains("push_turn_credentials_issued_total{mode=\"cloudflare\"} 5"),
        "{metrics}"
    );
    assert!(
        metrics.contains(
            "push_turn_provider_requests_total{provider=\"cloudflare\",outcome=\"ok\"} 3"
        ),
        "{metrics}"
    );
}

#[tokio::test]
async fn twilio_credentials_are_minted_with_the_shorter_lifetime_and_kept() {
    let (stub, url) = Stub::start().await;
    let settings = settings(&[
        ("PUSH_TURN_MODE", "external"),
        ("PUSH_TURN_PROVIDER", "twilio"),
        ("PUSH_TURN_TWILIO_ACCOUNT_SID", TWILIO_SID),
        ("PUSH_TURN_TWILIO_TOKEN", TWILIO_TOKEN),
    ])
    .unwrap()
    .unwrap();
    let turn = Turn::new(Some(settings))
        .unwrap()
        .with_provider_base_url(&url)
        .unwrap()
        .with_clock(|| at(1_800_000_000));
    let user = Uuid::now_v7();
    let first = turn.ice_servers(user).await.unwrap();
    let second = turn.ice_servers(user).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(stub.calls.load(Ordering::SeqCst), 1);
    assert_eq!(stub.ttl_seen.lock().unwrap().as_deref(), Some("Ttl=600"));
    assert_eq!(first.expires_at, at(1_800_000_300));
    assert_eq!(first.servers.len(), 2);
    assert_eq!(
        first.servers[1].urls,
        [
            "turn:global.turn.twilio.com:3478?transport=udp",
            "turn:global.turn.twilio.com:443?transport=tcp"
        ]
    );
}

/// Everything logged while the guard lives, at every level.
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let writer = Arc::clone(&buffer);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || Captured(Arc::clone(&writer)))
        .finish();
    (buffer, tracing::subscriber::set_default(subscriber))
}

#[tokio::test]
async fn provider_failures_are_unavailable_and_credentials_are_never_logged() {
    let (logs, _guard) = capture_logs();
    let (stub, url) = Stub::start().await;
    let turn = Turn::new(Some(cloudflare_settings()))
        .unwrap()
        .with_provider_base_url(&url)
        .unwrap();
    let user = Uuid::now_v7();
    let minted = turn.ice_servers(user).await.unwrap();
    assert!(minted.relay);

    *stub.status.lock().unwrap() = StatusCode::UNAUTHORIZED;
    assert_eq!(
        turn.ice_servers(Uuid::now_v7()).await,
        Err(AuthError::TurnUnavailable)
    );
    // An unreachable provider too.
    let unreachable = Turn::new(Some(cloudflare_settings()))
        .unwrap()
        .with_provider_base_url("http://127.0.0.1:9")
        .unwrap();
    assert_eq!(
        unreachable.ice_servers(user).await,
        Err(AuthError::TurnUnavailable)
    );
    // The REST scheme never logs either.
    Turn::new(Some(internal()))
        .unwrap()
        .ice_servers(user)
        .await
        .unwrap();

    let metrics = turn.render_metrics();
    assert!(
        metrics.contains("push_turn_credentials_failures_total{mode=\"cloudflare\"} 1"),
        "{metrics}"
    );
    assert!(metrics.contains("outcome=\"error\"} 1"), "{metrics}");
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("HTTP 401"), "the failure is logged: {logs}");
    for secret in [CLOUDFLARE_TOKEN, PROVIDER_CREDENTIAL, SECRET, "cf-user-"] {
        assert!(!logs.contains(secret), "{secret} logged: {logs}");
    }
}
