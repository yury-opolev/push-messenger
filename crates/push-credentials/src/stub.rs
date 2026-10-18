//! DEV ONLY test doubles for Entra ID credentials (feature `stub`): a stub of the
//! Microsoft Entra token endpoint and of the managed-identity endpoint (IMDS), served over
//! plain HTTP on 127.0.0.1 or over TLS (`serve_tls`), and helpers that make an issued
//! token a PostgreSQL role's or a Redis ACL user's password. Used by tests and the DEV
//! ONLY `push-dev-entra` rehearsal tool; never by a service binary. It builds on the MSRV
//! (the test CA and the containers, feature `testing`, need newer Rust).

use std::collections::HashMap;
use std::future::IntoFuture;
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
use serde_json::json;

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
    /// The federated token clients must present; `None` accepts any non-empty one.
    assertion: Option<String>,
    principals: Vec<StubPrincipal>,
    lifetime_s: AtomicU64,
    failing: AtomicBool,
    serial: AtomicU64,
    issued: Mutex<Vec<Issued>>,
    hook: Option<IssueHook>,
}

impl Stub {
    fn accepts(&self, assertion: &str) -> bool {
        match &self.assertion {
            Some(expected) => assertion == expected,
            None => !assertion.is_empty(),
        }
    }
}

/// The stub issuer.
pub struct StubIssuer {
    authority: String,
    state: Arc<Stub>,
}

impl StubIssuer {
    /// The issuer and its router, not yet served: tokens of `lifetime` (whole seconds, at
    /// least 1) for `principals` of `tenant` presenting `assertion` as their federated
    /// token (`None`: any non-empty one, e.g. a cluster's rotating service-account
    /// tokens). Whoever serves the router sets [`StubIssuer::with_authority`].
    pub fn new(
        tenant: &str,
        assertion: Option<&str>,
        principals: Vec<StubPrincipal>,
        lifetime: Duration,
        hook: Option<IssueHook>,
    ) -> (Self, Router) {
        let state = Arc::new(Stub {
            tenant: tenant.to_owned(),
            assertion: assertion.map(str::to_owned),
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
        let issuer = Self {
            authority: String::new(),
            state,
        };
        (issuer, router)
    }

    /// [`StubIssuer::new`] served over plain HTTP on 127.0.0.1 (the services accept an
    /// `http://` authority only on loopback).
    pub async fn start(
        tenant: &str,
        assertion: &str,
        principals: Vec<StubPrincipal>,
        lifetime: Duration,
        hook: Option<IssueHook>,
    ) -> Self {
        let (issuer, router) = Self::new(tenant, Some(assertion), principals, lifetime, hook);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, router).into_future());
        issuer.with_authority(authority)
    }

    /// Records where the router is served: the value for `AZURE_AUTHORITY_HOST`.
    pub fn with_authority(mut self, authority: String) -> Self {
        self.authority = authority;
        self
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
        || !stub.accepts(field("client_assertion"))
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

/// Serves `router` over TLS with `cert_pem` and `key_pem` (PEM) on `listener`, until the
/// process ends. Failed handshakes are dropped.
pub async fn serve_tls(
    listener: tokio::net::TcpListener,
    router: Router,
    cert_pem: &str,
    key_pem: &str,
) {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .expect("a PEM certificate");
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).expect("a PEM key");
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("a valid certificate and key");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    loop {
        let Ok((tcp, _)) = listener.accept().await else {
            continue;
        };
        let (acceptor, router) = (acceptor.clone(), router.clone());
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(tcp).await else {
                return;
            };
            let service = hyper::service::service_fn(move |request| {
                tower::ServiceExt::oneshot(router.clone(), request)
            });
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                    .await;
        });
    }
}

/// Makes `token` the password of `role`, as Azure accepts its tokens. Panics on failure.
pub async fn set_postgres_password(admin: &sqlx::PgPool, role: &str, token: &str) {
    try_set_postgres_password(admin, role, token).await.unwrap();
}

/// [`set_postgres_password`], returning the error. `admin` should not log statements:
/// the statement holds the token.
pub async fn try_set_postgres_password(
    admin: &sqlx::PgPool,
    role: &str,
    token: &str,
) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(&format!(
        "ALTER ROLE \"{role}\" WITH LOGIN PASSWORD '{token}'"
    ))
    .execute(admin)
    .await
    .map(drop)
}

/// Makes `token` the only password of ACL user `user` (all keys, channels, commands):
/// connections opened with an older token stay, new ones need this one. Panics on failure.
pub async fn set_redis_password(
    admin: &mut impl redis::aio::ConnectionLike,
    user: &str,
    token: &str,
) {
    try_set_redis_password(admin, user, token).await.unwrap();
}

/// [`set_redis_password`], returning the error.
pub async fn try_set_redis_password(
    admin: &mut impl redis::aio::ConnectionLike,
    user: &str,
    token: &str,
) -> redis::RedisResult<()> {
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
}
