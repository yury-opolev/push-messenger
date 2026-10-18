//! Entra ID sign-in against a real PostgreSQL (Docker must be running) and the stub
//! issuer: the service (`entra_accounts`), the HTTP API (`entra_api`) and the whole
//! service on sockets (`entra_server`). Here rather than under `tests\`: the stub is behind
//! the crate's `testing` feature, which the crate's own integration tests cannot enable.

mod entra_accounts;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::HeaderName;
use serde_json::Value;
use sqlx::PgPool;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};

use crate::api::{Api, router};
use crate::db::migrate_database;
use crate::directory::NoDirectory;
use crate::entra::testing::StubOpenId;
use crate::entra::{EntraSignIn, KeyTiming};
use crate::limits::{Limits, MemoryCounter};
use crate::links::LinkSigner;
use crate::password::Passwords;
use crate::service::{Auth, Settings, SignIn};
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
    let auth = Auth::new(
        pool.clone(),
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
    .with_sign_in(sign_in);
    let router = router(Api {
        auth: auth.clone(),
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: Some(HeaderName::from_static("x-real-ip")),
        directory: Arc::new(NoDirectory),
    });
    World {
        _container: container,
        pool,
        auth,
        router,
        stub,
    }
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
