//! Test doubles for Entra ID credentials (feature `testing`): a stub of the Microsoft
//! Entra token endpoint and of the managed-identity endpoint (IMDS), a test certificate
//! authority, and PostgreSQL and Redis containers that require TLS. Never used by a
//! service binary.

use std::collections::HashMap;
use std::future::IntoFuture;
use std::path::{Path as FilePath, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::future::BoxFuture;
use rcgen::{BasicConstraints, Certificate, CertificateParams, DnType, IsCa, KeyPair};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use testcontainers_modules::testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, GenericImage, ImageExt};

const JWT_BEARER: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

/// A principal the stub issues tokens for: an identity's client id and object id.
#[derive(Debug, Clone)]
pub struct StubPrincipal {
    pub client_id: String,
    pub object_id: String,
}

/// One token the stub issued.
#[derive(Debug, Clone)]
pub struct Issued {
    pub client_id: String,
    pub object_id: String,
    pub scope: String,
    pub token: String,
}

/// Runs before the stub answers with a token, e.g. to make it the datastore password.
pub type IssueHook = Arc<dyn Fn(Issued) -> BoxFuture<'static, ()> + Send + Sync>;

struct Stub {
    tenant: String,
    assertion: String,
    principals: Vec<StubPrincipal>,
    lifetime_s: AtomicU64,
    failing: AtomicBool,
    serial: AtomicU64,
    issued: Mutex<Vec<Issued>>,
    hook: Option<IssueHook>,
}

/// The stub issuer, listening on 127.0.0.1.
pub struct StubIssuer {
    authority: String,
    state: Arc<Stub>,
}

impl StubIssuer {
    /// Issues tokens of `lifetime` (whole seconds, at least 1) to `principals` of
    /// `tenant` that present `assertion` as their federated token.
    pub async fn start(
        tenant: &str,
        assertion: &str,
        principals: Vec<StubPrincipal>,
        lifetime: Duration,
        hook: Option<IssueHook>,
    ) -> Self {
        let state = Arc::new(Stub {
            tenant: tenant.to_owned(),
            assertion: assertion.to_owned(),
            principals,
            lifetime_s: AtomicU64::new(lifetime.as_secs().max(1)),
            failing: AtomicBool::new(false),
            serial: AtomicU64::new(0),
            issued: Mutex::new(Vec::new()),
            hook,
        });
        let router = Router::new()
            .route("/{tenant}/oauth2/v2.0/token", post(token))
            .route("/metadata/identity/oauth2/token", get(imds))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, router).into_future());
        Self { authority, state }
    }

    /// The value for `AZURE_AUTHORITY_HOST`, with a trailing `/`.
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// The managed-identity endpoint, for `ManagedIdentity::new`.
    pub fn imds_endpoint(&self) -> String {
        format!("{}metadata/identity/oauth2/token", self.authority)
    }

    /// Makes every request fail with 503 until called again with `false`.
    pub fn set_failing(&self, failing: bool) {
        self.state.failing.store(failing, Ordering::SeqCst);
    }

    /// Every token issued so far, in order.
    pub fn issued(&self) -> Vec<Issued> {
        self.state.issued.lock().unwrap().clone()
    }
}

async fn token(
    State(stub): State<Arc<Stub>>,
    Path(tenant): Path<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if stub.failing.load(Ordering::SeqCst) {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    let field = |name: &str| form.get(name).map(String::as_str).unwrap_or_default();
    if tenant != stub.tenant
        || field("grant_type") != "client_credentials"
        || field("client_assertion_type") != JWT_BEARER
        || field("client_assertion") != stub.assertion
    {
        return refuse(StatusCode::UNAUTHORIZED, "invalid_client");
    }
    issue(&stub, field("client_id"), field("scope"), false).await
}

async fn imds(
    State(stub): State<Arc<Stub>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if stub.failing.load(Ordering::SeqCst) {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    }
    if headers
        .get("metadata")
        .and_then(|value| value.to_str().ok())
        != Some("true")
    {
        return refuse(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let client_id = match query.get("client_id") {
        Some(client_id) => client_id.clone(),
        None => stub.principals[0].client_id.clone(),
    };
    let scope = format!(
        "{}/.default",
        query.get("resource").cloned().unwrap_or_default()
    );
    issue(&stub, &client_id, &scope, true).await
}

async fn issue(stub: &Stub, client_id: &str, scope: &str, lifetime_as_text: bool) -> Response {
    let Some(principal) = stub.principals.iter().find(|p| p.client_id == client_id) else {
        return refuse(StatusCode::BAD_REQUEST, "unauthorized_client");
    };
    if !scope.ends_with("/.default") || scope.len() <= "/.default".len() {
        return refuse(StatusCode::BAD_REQUEST, "invalid_scope");
    }
    let serial = stub.serial.fetch_add(1, Ordering::SeqCst);
    let claims = json!({"oid": principal.object_id, "appid": client_id, "n": serial});
    let token = format!(
        "{}.{}.stub",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let issued = Issued {
        client_id: client_id.to_owned(),
        object_id: principal.object_id.clone(),
        scope: scope.to_owned(),
        token: token.clone(),
    };
    if let Some(hook) = &stub.hook {
        hook(issued.clone()).await;
    }
    stub.issued.lock().unwrap().push(issued);
    let lifetime = stub.lifetime_s.load(Ordering::SeqCst);
    let expires_in = if lifetime_as_text {
        json!(lifetime.to_string())
    } else {
        json!(lifetime)
    };
    Json(json!({"token_type": "Bearer", "expires_in": expires_in, "access_token": token}))
        .into_response()
}

/// An OAuth error answer whose description echoes request data, as real ones may: the
/// sources must never pass it on.
fn refuse(status: StatusCode, code: &str) -> Response {
    let body = json!({
        "error": code,
        "error_description": "AADSTS700016: client_assertion=leaked-assertion",
    });
    (status, Json(body)).into_response()
}

// ---- TLS test servers ----

/// A throwaway certificate authority; its certificate is also written to a file, as
/// `PUSH_DATABASE_CA_FILE` and `PUSH_REDIS_CA_FILE` expect.
pub struct TestCa {
    cert: Certificate,
    key: KeyPair,
    file: PathBuf,
}

impl TestCa {
    pub fn new() -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "push-messenger test CA");
        let cert = params.self_signed(&key).unwrap();
        let file = std::env::temp_dir().join(format!("push-test-ca-{}.pem", uuid::Uuid::now_v7()));
        std::fs::write(&file, cert.pem()).unwrap();
        Self { cert, key, file }
    }

    pub fn pem(&self) -> String {
        self.cert.pem()
    }

    pub fn file(&self) -> &FilePath {
        &self.file
    }

    /// A server certificate for `127.0.0.1` and `localhost`: (certificate, key), PEM.
    pub fn server(&self) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        // rcgen turns "127.0.0.1" into an IP address name.
        let params =
            CertificateParams::new(vec!["127.0.0.1".to_owned(), "localhost".to_owned()]).unwrap();
        let cert = params.signed_by(&key, &self.cert, &self.key).unwrap();
        (cert.pem(), key.serialize_pem())
    }
}

impl Default for TestCa {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TestCa {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

/// The superuser password of [`tls_postgres`].
pub const POSTGRES_ADMIN_PASSWORD: &str = "admin-password";

/// PostgreSQL 17 that offers TLS with a certificate of the test CA.
pub struct TlsPostgres {
    _container: ContainerAsync<GenericImage>,
    pub port: u16,
}

/// Starts it. The key must belong to `postgres` with mode 0600, so the entrypoint copies
/// the files into place before handing over to the image's own entrypoint.
pub async fn tls_postgres(ca: &TestCa) -> TlsPostgres {
    let (cert, key) = ca.server();
    let script = "install -o postgres -m 0600 /tls/server.key /var/lib/postgresql/server.key \
        && install -o postgres -m 0644 /tls/server.crt /var/lib/postgresql/server.crt \
        && exec docker-entrypoint.sh postgres -c ssl=on \
        -c ssl_cert_file=/var/lib/postgresql/server.crt \
        -c ssl_key_file=/var/lib/postgresql/server.key";
    let container = GenericImage::new("postgres", "17-alpine")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_entrypoint("sh")
        .with_env_var("POSTGRES_PASSWORD", POSTGRES_ADMIN_PASSWORD)
        .with_copy_to("/tls/server.crt", cert.into_bytes())
        .with_copy_to("/tls/server.key", key.into_bytes())
        .with_cmd(["-c", script])
        .start()
        .await
        .expect("start PostgreSQL with TLS (is Docker running?)");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    TlsPostgres {
        _container: container,
        port,
    }
}

impl TlsPostgres {
    /// Connect options for `user` with full verification against `ca`.
    pub fn options(&self, ca: &TestCa, user: &str, database: &str) -> PgConnectOptions {
        PgConnectOptions::new_without_pgpass()
            .host("127.0.0.1")
            .port(self.port)
            .username(user)
            .database(database)
            .ssl_mode(PgSslMode::VerifyFull)
            .ssl_root_cert(ca.file())
    }

    /// A superuser pool, once the server accepts connections (it restarts once after
    /// initialising, so the first minute is retried).
    pub async fn admin(&self, ca: &TestCa) -> sqlx::PgPool {
        for _ in 0..120 {
            if let Ok(pool) = self.try_admin(ca).await {
                return pool;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        panic!("PostgreSQL with TLS did not accept connections within 60 s");
    }

    pub async fn try_admin(&self, ca: &TestCa) -> Result<sqlx::PgPool, sqlx::Error> {
        // Statements are not logged: this pool sets the stub's tokens as passwords, and a
        // test that captures the services' logs must not find them there.
        let options = sqlx::ConnectOptions::disable_statement_logging(
            self.options(ca, "postgres", "postgres")
                .password(POSTGRES_ADMIN_PASSWORD),
        );
        PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options)
            .await
    }
}

/// Makes `token` the password of `role`, as Azure accepts its tokens.
pub async fn set_postgres_password(admin: &sqlx::PgPool, role: &str, token: &str) {
    sqlx::raw_sql(&format!(
        "ALTER ROLE \"{role}\" WITH LOGIN PASSWORD '{token}'"
    ))
    .execute(admin)
    .await
    .unwrap();
}

/// Redis 7 listening only on TLS (no plain port), with a certificate of the test CA.
pub struct TlsRedis {
    _container: ContainerAsync<GenericImage>,
    pub port: u16,
}

pub async fn tls_redis(ca: &TestCa) -> TlsRedis {
    let (cert, key) = ca.server();
    let container = GenericImage::new("redis", "7-alpine")
        .with_exposed_port(6379.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .with_copy_to("/tls/server.crt", cert.into_bytes())
        .with_copy_to("/tls/server.key", key.into_bytes())
        .with_cmd([
            "redis-server",
            "--port",
            "0",
            "--tls-port",
            "6379",
            "--tls-cert-file",
            "/tls/server.crt",
            "--tls-key-file",
            "/tls/server.key",
            "--tls-auth-clients",
            "no",
        ])
        .start()
        .await
        .expect("start Redis with TLS (is Docker running?)");
    let port = container.get_host_port_ipv4(6379).await.unwrap();
    TlsRedis {
        _container: container,
        port,
    }
}

impl TlsRedis {
    /// `rediss://127.0.0.1:{port}`, without credentials.
    pub fn url(&self) -> String {
        format!("rediss://127.0.0.1:{}", self.port)
    }

    /// The `default` (password-less) user, trusting `ca`.
    pub async fn admin(&self, ca: &TestCa) -> redis::aio::MultiplexedConnection {
        let certificates = redis::TlsCertificates {
            client_tls: None,
            root_cert: Some(ca.pem().into_bytes()),
        };
        redis::Client::build_with_tls(self.url(), certificates)
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .unwrap()
    }
}

/// Makes `token` the only password of ACL user `user` (all keys, channels, commands):
/// connections opened with an older token stay, new ones need this one.
pub async fn set_redis_password(
    admin: &mut redis::aio::MultiplexedConnection,
    user: &str,
    token: &str,
) {
    redis::cmd("ACL")
        .arg("SETUSER")
        .arg(user)
        .arg("on")
        .arg("resetpass")
        .arg(format!(">{token}"))
        .arg("~*")
        .arg("&*")
        .arg("+@all")
        .exec_async(admin)
        .await
        .unwrap();
}
