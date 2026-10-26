//! Entra ID sign-in against a real PostgreSQL (Docker must be running) and the stub
//! issuer: the service (`entra_accounts`), the HTTP API (`entra_api`) and the whole
//! service on sockets (`entra_server`). Here rather than under `tests\`: the stub is behind
//! the crate's `testing` feature, which the crate's own integration tests cannot enable.

mod admin;
mod agent_key;
mod agents;
mod entra_accounts;
mod entra_api;
mod entra_server;
mod identity;
mod identity_accounts;
mod identity_api;
mod identity_server;
mod switch;
mod templates;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::http::HeaderName;
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use uuid::Uuid;

use crate::api::{Api, router};
use crate::db::migrate_database;
use crate::devices::Tokens;
use crate::directory::NoDirectory;
use crate::entra::testing::{StubOpenId, TENANT};
use crate::entra::{EntraIdentity, EntraSignIn, KeyTiming};
use crate::identity::{
    Admission, AdmissionDecision, AuthenticationError, Authenticator, Principal,
};
use crate::limits::{Limits, MemoryCounter};
use crate::links::LinkSigner;
use crate::password::Passwords;
use crate::service::{Auth, Settings, SignIn, Signup};
use crate::tokens::AccessTokens;

pub(crate) const AUDIENCE: &str = "ws://push.local/v1/socket";
/// The link key of every world.
pub(crate) const LINK_KEY: [u8; 32] = [3; 32];

/// Which ways to sign in a world offers.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Offer {
    Password,
    Entra,
    Both,
}

pub(crate) struct World {
    _container: ContainerAsync<Postgres>,
    pub pool: PgPool,
    pub auth: Auth,
    pub router: Router,
    pub stub: StubOpenId,
}

/// Key timings without waits, so tests see every fetch at once.
pub(crate) fn quick() -> KeyTiming {
    KeyTiming {
        unknown_kid_cooldown: Duration::ZERO,
        retry_after_failure: Duration::ZERO,
        fetch_timeout: Duration::from_secs(2),
        ..KeyTiming::default()
    }
}

/// A migrated database, the stub issuer, the service offering `offer` and its router
/// (in-memory rate limits, the client IP in `x-real-ip`).
pub(crate) async fn world(offer: Offer) -> World {
    world_with(offer, None, None).await
}

/// As [`world`], also offering `external` and asking `admission`.
pub(crate) async fn world_with(
    offer: Offer,
    external: Option<Arc<dyn Authenticator>>,
    admission: Option<Arc<dyn Admission>>,
) -> World {
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
    let stub = StubOpenId::start().await;
    let entra = EntraSignIn::with_timing(stub.settings(), quick()).unwrap();
    let sign_in = match offer {
        Offer::Password => SignIn::default(),
        Offer::Entra => SignIn {
            password: false,
            entra: Some(entra),
        },
        Offer::Both => SignIn {
            password: true,
            entra: Some(entra),
        },
    };
    let mut auth = service(pool.clone(), sign_in);
    if let Some(external) = external {
        auth = auth.with_external(external);
    }
    if let Some(admission) = admission {
        auth = auth.with_admission(admission);
    }
    let router = router(Api {
        auth: auth.clone(),
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: Some(HeaderName::from_static("x-real-ip")),
        directory: Arc::new(NoDirectory),
        templates: Arc::new(crate::templates::NoTemplates),
    });
    World {
        _container: container,
        pool,
        auth,
        router,
        stub,
    }
}

/// The service on `pool` offering `sign_in`, with the settings every world uses.
pub(crate) fn service(pool: PgPool, sign_in: SignIn) -> Auth {
    Auth::new(
        pool,
        Settings {
            public_url: "http://push.local".to_owned(),
            passwords: Passwords::new(64, 1, 1).unwrap(),
            links: LinkSigner::new(LINK_KEY.to_vec()).unwrap(),
            tokens: AccessTokens::from_seed(&[5; 32], AUDIENCE).unwrap(),
            hash_concurrency: 4,
            hash_queue: 64,
            hash_wait: Duration::from_secs(5),
        },
    )
    .with_sign_in(sign_in)
}

/// A verified email/password account `name@example.com` (handle `name`) with `devices`
/// registered devices: their tokens. `auth` must offer password sign-in.
pub(crate) async fn password_user(
    auth: &Auth,
    pool: &PgPool,
    name: &str,
    devices: usize,
) -> Vec<Tokens> {
    let email = format!("{name}@example.com");
    auth.signup(Signup {
        email: email.clone(),
        password: "correct horse".to_owned(),
        handle: name.to_owned(),
        display_name: name.to_owned(),
    })
    .await
    .unwrap();
    let link: String = sqlx::query_scalar(
        "SELECT payload->'params'->>'link' FROM auth.outbox WHERE kind = 'email.send' \
         AND payload->>'to' = $1 ORDER BY created_at DESC, event_id DESC LIMIT 1",
    )
    .bind(&email)
    .fetch_one(pool)
    .await
    .unwrap();
    auth.verify_email(link.split_once("?token=").unwrap().1)
        .await
        .unwrap();
    let mut tokens = Vec::new();
    for _ in 0..devices {
        let registration = auth.login(&email, "correct horse").await.unwrap().token;
        tokens.push(
            auth.register_device(&registration, Uuid::now_v7(), "Phone", "ios")
                .await
                .unwrap(),
        );
    }
    tokens
}

/// An Entra ID user of the stub's tenant with one registered device: its tokens.
pub(crate) async fn entra_user(auth: &Auth, object_id: Uuid) -> Tokens {
    let identity = EntraIdentity {
        tenant_id: Uuid::parse_str(TENANT).unwrap(),
        object_id,
        name: Some("Erin".to_owned()),
        email: None,
    };
    let signed_in = auth.sign_in_entra(&identity, None).await.unwrap();
    auth.register_device(
        &signed_in.registration.token,
        Uuid::now_v7(),
        "Laptop",
        "web",
    )
    .await
    .unwrap()
}

impl World {
    /// The outbox, oldest first: `(kind, payload)`.
    pub async fn outbox(&self) -> Vec<(String, Value)> {
        sqlx::query_as("SELECT kind, payload FROM auth.outbox ORDER BY created_at, event_id")
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }

    /// How many accounts exist.
    pub async fn accounts(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM auth.user_account")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

/// A test authenticator: `ok|issuer|subject|email|group,group` names that user (name
/// `Acme User`, claims `iss`, `sub` and `groups`), `foreign|…` the same user of the
/// provider `foreign`, `down` is unavailable, anything else is refused (`signature`). It
/// counts its calls; its sessions last two hours.
#[derive(Debug)]
pub(crate) struct FakeIdentity {
    pub provider: String,
    pub calls: AtomicUsize,
}

impl FakeIdentity {
    pub fn new(provider: &str) -> Arc<Self> {
        Arc::new(Self {
            provider: provider.to_owned(),
            calls: AtomicUsize::new(0),
        })
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// A token [`FakeIdentity`] accepts; `groups` is comma-separated.
pub(crate) fn fake_token(issuer: &str, subject: &str, email: &str, groups: &str) -> String {
    format!("ok|{issuer}|{subject}|{email}|{groups}")
}

#[async_trait]
impl Authenticator for FakeIdentity {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn max_session(&self) -> Duration {
        Duration::from_secs(2 * 3600)
    }

    async fn authenticate(&self, token: &str) -> Result<Principal, AuthenticationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let parts: Vec<&str> = token.split('|').collect();
        match parts.as_slice() {
            [kind @ ("ok" | "foreign"), issuer, subject, email, groups] => {
                let groups: Vec<&str> = groups.split(',').filter(|g| !g.is_empty()).collect();
                let claims = json!({"iss": issuer, "sub": subject, "groups": groups});
                Ok(Principal {
                    provider: if *kind == "ok" {
                        self.provider.clone()
                    } else {
                        "foreign".to_owned()
                    },
                    issuer: (*issuer).to_owned(),
                    subject: (*subject).to_owned(),
                    name: Some("Acme User".to_owned()),
                    email: (!email.is_empty()).then(|| (*email).to_owned()),
                    claims: claims.as_object().cloned().unwrap_or_default(),
                })
            }
            ["down"] => Err(AuthenticationError::Unavailable(
                "the fake provider is down".to_owned(),
            )),
            _ => Err(AuthenticationError::Rejected("signature")),
        }
    }
}

/// An admission requiring `group` in the `groups` claim, `Unavailable` while `down`. It
/// keeps the claims it saw.
#[derive(Debug)]
pub(crate) struct RequireGroup {
    pub group: String,
    pub down: AtomicBool,
    pub seen: Mutex<Vec<Map<String, Value>>>,
}

impl RequireGroup {
    pub fn new(group: &str) -> Arc<Self> {
        Arc::new(Self {
            group: group.to_owned(),
            down: AtomicBool::new(false),
            seen: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl Admission for RequireGroup {
    async fn admit(&self, principal: &Principal) -> AdmissionDecision {
        self.seen.lock().unwrap().push(principal.claims.clone());
        if self.down.load(Ordering::SeqCst) {
            return AdmissionDecision::Unavailable;
        }
        let member = principal
            .claims
            .get("groups")
            .and_then(Value::as_array)
            .is_some_and(|groups| groups.iter().any(|group| group == self.group.as_str()));
        if member {
            AdmissionDecision::Admit
        } else {
            AdmissionDecision::Refuse
        }
    }
}

/// Every log line of this test binary, as JSON.
static LINES: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct Capture;

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        LINES.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Captures every log line of the process from now on, as JSON: the global subscriber,
/// since a thread's own misses events while other tests run in parallel. Several tests
/// share it, so each looks for its own values.
pub(crate) fn capture_logs() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(|| Capture)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
    });
}

/// What was captured so far.
pub(crate) fn captured_logs() -> String {
    String::from_utf8_lossy(&LINES.lock().unwrap()).into_owned()
}

/// The fields of every captured `security` event.
pub(crate) fn security_events() -> Vec<Value> {
    captured_logs()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["target"] == "security")
        .map(|line| line["fields"].clone())
        .collect()
}
