//! DEV ONLY: tools for the Entra ID rehearsals on a local cluster (Plan 12B,
//! `deploy/minikube/entra-rehearsal.ps1`; Plan 13B,
//! `deploy/minikube/entra-sign-in-rehearsal.ps1`). Never in a service image and never
//! against real datastores or a real tenant.
//!
//!   push-dev-entra issuer    an HTTPS stub of Microsoft Entra ID's token endpoint
//!   push-dev-entra sign-in   an HTTPS stub of Microsoft Entra ID sign-in (Plan 13B)
//!
//! The test CA and the certificates come from the `dev-entra-certs` example of this crate
//! (`cargo run -p push-smoke --example dev-entra-certs -- <dir> <dns name>...`): the test
//! CA needs newer Rust than the MSRV binaries are built with.
//!
//! `issuer` serves the stub token endpoint on :8443 over TLS and reads:
//!
//! | Variable | Meaning |
//! |---|---|
//! | `PUSH_DEV_ENTRA_TLS_DIR` | directory with `ca.crt`, `server.crt` and `server.key` |
//! | `PUSH_DEV_ENTRA_TENANT` | the tenant id the services name |
//! | `PUSH_DEV_ENTRA_PRINCIPALS` | `client id:PostgreSQL role:object id`, comma-separated |
//! | `PUSH_DEV_ENTRA_LIFETIME_S` | the tokens' lifetime in seconds |
//! | `PUSH_DEV_ENTRA_POSTGRES_HOST`, `PUSH_DEV_ENTRA_REDIS_HOST`, `PUSH_DEV_ENTRA_REDIS_PORT` | the rehearsal's TLS datastores |
//! | `PUSH_DEV_ENTRA_ADMIN_PASSWORD` | their administrator's password (`postgres`, Redis `default`) |
//!
//! Before answering, it makes every token it issues the principal's PostgreSQL role
//! password (PostgreSQL scope) or one of its Redis ACL user's passwords until the token
//! expires (Redis scope, the user being the object id), as Azure accepts its own tokens.
//! PostgreSQL keeps one password per role, so there only a principal's latest token opens
//! new connections: two pods of one principal (during a rollout) can briefly fail to
//! connect, which Azure would not do; the rehearsal waits for rollouts to settle. It
//! accepts any non-empty federated token: in the cluster that is the pod's projected
//! service-account token. It logs one line per token, never the token.
//!
//! `sign-in` serves Plan 13A's stub OpenID metadata and key set (`push_auth_core::entra::testing`)
//! over TLS for auth-svc, and `POST /dev/token {"oid", "tenant"?}` over plain HTTP, which
//! mints a delegated user token for the stub's API with the **public** test key: whoever
//! reaches it signs in as anyone. It reads:
//!
//! | Variable | Meaning |
//! |---|---|
//! | `PUSH_DEV_ENTRA_TLS_DIR` | directory with `server.crt` and `server.key` |
//! | `PUSH_DEV_SIGN_IN_AUTHORITY` | the authority auth-svc trusts, `https://<service>.<namespace>.svc/` |
//! | `PUSH_DEV_SIGN_IN_TLS_PORT` | the metadata port (default 8443; the rehearsal uses 443, the port egress policies open) |
//! | `PUSH_DEV_SIGN_IN_TOKEN_PORT` | the token endpoint's port (default 8080) |
//!
//! It logs one line per minted token, never the token.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures_util::future::BoxFuture;
use push_credentials::stub::{
    IssueHook, Issued, StubIssuer, StubPrincipal, serve_tls, try_set_postgres_password,
};
use sqlx::ConnectOptions;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};

const POSTGRES_SCOPE: &str = "https://ossrdbms-aad.database.windows.net/.default";

fn var(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| format!("{name} must be set"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    push_credentials::install_crypto_provider();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("issuer") => issuer().await,
        Some("sign-in") => sign_in().await,
        _ => anyhow::bail!("usage: push-dev-entra issuer | push-dev-entra sign-in (DEV ONLY)"),
    }
}

/// One principal: its client id, PostgreSQL role and object id (the Redis ACL user).
struct Principal {
    client_id: String,
    role: String,
    object_id: String,
}

fn principals(spec: &str) -> anyhow::Result<Vec<Principal>> {
    spec.split(',')
        .map(
            |entry| match entry.trim().split(':').collect::<Vec<_>>()[..] {
                [client_id, role, object_id] => Ok(Principal {
                    client_id: client_id.to_owned(),
                    role: role.to_owned(),
                    object_id: object_id.to_owned(),
                }),
                _ => anyhow::bail!("PUSH_DEV_ENTRA_PRINCIPALS: expected client:role:oid entries"),
            },
        )
        .collect()
}

/// Retries `connect` every 2 s for up to 2 minutes: the datastores may start later.
async fn patiently<T, F, Fut>(what: &str, connect: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut attempt = 0;
    loop {
        match connect().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt < 60 => {
                attempt += 1;
                println!("{what} not reachable yet ({error:#}); retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(error) => return Err(error.context(format!("{what} not reachable"))),
        }
    }
}

async fn issuer() -> anyhow::Result<()> {
    let principals = Arc::new(principals(&var("PUSH_DEV_ENTRA_PRINCIPALS")?)?);
    let tls = std::path::PathBuf::from(var("PUSH_DEV_ENTRA_TLS_DIR")?);
    let ca = tls.join("ca.crt");
    let password = var("PUSH_DEV_ENTRA_ADMIN_PASSWORD")?;

    // Statements are not logged: they carry the tokens.
    let postgres_options = PgConnectOptions::new_without_pgpass()
        .host(&var("PUSH_DEV_ENTRA_POSTGRES_HOST")?)
        .port(5432)
        .username("postgres")
        .password(&password)
        .database("postgres")
        .ssl_mode(PgSslMode::VerifyFull)
        .ssl_root_cert(&ca)
        .disable_statement_logging();
    let postgres = patiently("PostgreSQL", || {
        let options = postgres_options.clone();
        async move {
            Ok(PgPoolOptions::new()
                .max_connections(2)
                .acquire_timeout(Duration::from_secs(5))
                .connect_with(options)
                .await?)
        }
    })
    .await?;

    let redis_url = format!(
        "rediss://:{password}@{}:{}/",
        var("PUSH_DEV_ENTRA_REDIS_HOST")?,
        var("PUSH_DEV_ENTRA_REDIS_PORT")?
    );
    let certificates = redis::TlsCertificates {
        client_tls: None,
        root_cert: Some(std::fs::read(&ca)?),
    };
    let redis_client = redis::Client::build_with_tls(redis_url, certificates)?;
    // A connection manager reconnects by itself if Redis restarts.
    let redis = patiently("Redis", || {
        let client = redis_client.clone();
        async move { Ok(client.get_connection_manager().await?) }
    })
    .await?;

    let lifetime = Duration::from_secs(var("PUSH_DEV_ENTRA_LIFETIME_S")?.parse()?);
    let roles = Arc::clone(&principals);
    let hook: IssueHook = Arc::new(move |issued: Issued| {
        let (postgres, redis, roles) = (postgres.clone(), redis.clone(), Arc::clone(&roles));
        Box::pin(async move {
            let set = if issued.scope == POSTGRES_SCOPE {
                match roles.iter().find(|p| p.client_id == issued.client_id) {
                    Some(principal) => {
                        try_set_postgres_password(&postgres, &principal.role, &issued.token)
                            .await
                            .map_err(|error| error.to_string())
                    }
                    None => Err("no PostgreSQL role for this client".to_owned()),
                }
            } else {
                add_redis_password(redis, &issued.object_id, &issued.token, lifetime)
                    .await
                    .map_err(|error| error.to_string())
            };
            match set {
                Ok(()) => println!("issued a token for {} ({})", issued.client_id, issued.scope),
                Err(error) => println!(
                    "issued a token for {} ({}) but could not make it the password: {error}",
                    issued.client_id, issued.scope
                ),
            }
        }) as BoxFuture<'static, ()>
    });

    let stub_principals = principals
        .iter()
        .map(|p| StubPrincipal {
            client_id: p.client_id.clone(),
            object_id: p.object_id.clone(),
        })
        .collect();
    let tenant = var("PUSH_DEV_ENTRA_TENANT")?;
    let (_issuer, router) = StubIssuer::new(&tenant, None, stub_principals, lifetime, Some(hook));
    let cert = std::fs::read_to_string(tls.join("server.crt"))?;
    let key = std::fs::read_to_string(tls.join("server.key"))?;
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8443").await?;
    println!("serving the DEV ONLY Entra ID stub on :8443");
    serve_tls(listener, router, &cert, &key).await;
    Ok(())
}

fn port(name: &str, default: u16) -> anyhow::Result<u16> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .with_context(|| format!("{name} must be a port number")),
        Err(_) => Ok(default),
    }
}

/// DEV ONLY: Microsoft Entra ID sign-in for the Plan 13B rehearsal (see the module docs).
async fn sign_in() -> anyhow::Result<()> {
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::{Json, Router};
    use push_auth_core::entra::testing::{KEY_A, StubOpenId, TENANT, sign};
    use serde_json::{Value, json};

    let authority = var("PUSH_DEV_SIGN_IN_AUTHORITY")?;
    let (stub, metadata) = StubOpenId::with_authority(&authority);
    let mint = Router::new()
        .route(
            "/dev/token",
            post(
                |State(stub): State<Arc<StubOpenId>>, Json(body): Json<Value>| async move {
                    let oid = body["oid"].as_str().unwrap_or_default();
                    let tenant = body["tenant"].as_str().unwrap_or(TENANT);
                    if uuid::Uuid::try_parse(oid).is_err() || uuid::Uuid::try_parse(tenant).is_err()
                    {
                        return Err(StatusCode::BAD_REQUEST);
                    }
                    let token = sign(KEY_A, &stub.user_claims(tenant, oid));
                    println!("minted a DEV ONLY Entra ID user token for tenant {tenant}");
                    Ok(Json(json!({ "token": token })))
                },
            ),
        )
        .with_state(Arc::new(stub));
    let tls = std::path::PathBuf::from(var("PUSH_DEV_ENTRA_TLS_DIR")?);
    let cert = std::fs::read_to_string(tls.join("server.crt"))?;
    let key = std::fs::read_to_string(tls.join("server.key"))?;
    let tls_port = port("PUSH_DEV_SIGN_IN_TLS_PORT", 8443)?;
    let token_port = port("PUSH_DEV_SIGN_IN_TOKEN_PORT", 8080)?;
    let plain = tokio::net::TcpListener::bind(("0.0.0.0", token_port)).await?;
    tokio::spawn(axum::serve(plain, mint).into_future());
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", tls_port)).await?;
    println!(
        "serving the DEV ONLY Entra ID sign-in stub for {authority}: metadata and keys on :{tls_port} (TLS), user tokens on :{token_port}"
    );
    serve_tls(listener, metadata, &cert, &key).await;
    Ok(())
}

/// Adds `token` to the passwords of ACL user `user` (all keys, channels, commands) and
/// removes it once it expires: as on Azure, every unexpired token opens connections.
async fn add_redis_password(
    mut redis: redis::aio::ConnectionManager,
    user: &str,
    token: &str,
    lifetime: Duration,
) -> redis::RedisResult<()> {
    redis::cmd("ACL")
        .arg("SETUSER")
        .arg(user)
        .arg("on")
        .arg(format!(">{token}"))
        .arg("~*")
        .arg("&*")
        .arg("+@all")
        .exec_async(&mut redis)
        .await?;
    let (user, token) = (user.to_owned(), token.to_owned());
    tokio::spawn(async move {
        tokio::time::sleep(lifetime).await;
        let removed = redis::cmd("ACL")
            .arg("SETUSER")
            .arg(&user)
            .arg(format!("<{token}"))
            .exec_async(&mut redis)
            .await;
        if let Err(error) = removed {
            println!("could not remove an expired Redis password of {user}: {error}");
        }
    });
    Ok(())
}
