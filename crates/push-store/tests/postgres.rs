//! The behaviour suite against PostgreSQL.
//!
//! The first case to run starts one Postgres container for the whole run (Docker must be
//! running) and migrates a template database. Every case then gets a database of its own,
//! copied from the template, so cases run in parallel; it is dropped when the case passes.
//! The container is removed when the run ends.

use std::sync::{Arc, Mutex, OnceLock};

use push_store::PgStore;
use push_testkit::Backend;
use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};
use testcontainers_modules::postgres::Postgres as PostgresImage;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::runtime::Handle;
use uuid::Uuid;

/// The migrated database every case's database is copied from.
const TEMPLATE: &str = "push_template";

/// The running container and the URL of its server, without a database name.
struct Server {
    container: ContainerAsync<PostgresImage>,
    url: String,
}

async fn start_server() -> Server {
    let container = PostgresImage::default()
        .with_tag("17-alpine")
        .with_cmd(["-c", "fsync=off", "-c", "max_connections=500"])
        .start()
        .await
        .expect("start a Postgres container (is Docker running?)");
    let host = container.get_host().await.expect("the container host");
    // Docker publishes ports on IPv4; "localhost" would try IPv6 first on every connect.
    let host = match host.to_string().as_str() {
        "localhost" => "127.0.0.1".to_owned(),
        other => other.to_owned(),
    };
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("the Postgres port");
    let url = format!("postgres://postgres:postgres@{host}:{port}");

    let mut admin = PgConnection::connect(&format!("{url}/postgres"))
        .await
        .expect("connect to Postgres");
    admin
        .execute(format!("CREATE DATABASE {TEMPLATE}").as_str())
        .await
        .expect("create the template database");
    admin.close().await.expect("disconnect");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{url}/{TEMPLATE}"))
        .await
        .expect("connect to the template database");
    PgStore::new(pool.clone())
        .migrate()
        .await
        .expect("migrate the template database");
    // CREATE DATABASE … TEMPLATE fails while anyone is connected to the template.
    pool.close().await;
    Server { container, url }
}

/// Makes a database per store and drops them when the case passes.
#[derive(Clone)]
struct Postgres {
    url: String,
    databases: Arc<Mutex<Vec<String>>>,
}

impl Postgres {
    async fn admin(&self) -> PgConnection {
        PgConnection::connect(&format!("{}/postgres", self.url))
            .await
            .expect("connect to Postgres")
    }
}

impl Backend for Postgres {
    type Store = PgStore;

    async fn store(&self) -> PgStore {
        let name = format!("case_{}", Uuid::new_v4().simple());
        let mut admin = self.admin().await;
        admin
            .execute(format!("CREATE DATABASE {name} TEMPLATE {TEMPLATE}").as_str())
            .await
            .expect("create a database for the case");
        admin.close().await.expect("disconnect");
        self.databases.lock().unwrap().push(name.clone());
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(&format!("{}/{name}", self.url))
            .await
            .expect("connect to the case's database");
        PgStore::new(pool)
    }

    async fn finish(&self) {
        let names = std::mem::take(&mut *self.databases.lock().unwrap());
        let mut admin = self.admin().await;
        for name in names {
            admin
                .execute(format!("DROP DATABASE {name} WITH (FORCE)").as_str())
                .await
                .expect("drop the case's database");
        }
        admin.close().await.expect("disconnect");
    }
}

fn main() {
    let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
    let handle: Handle = runtime.handle().clone();
    let server: Arc<OnceLock<Server>> = Arc::new(OnceLock::new());

    let shared = Arc::clone(&server);
    let backend = move || Postgres {
        url: shared
            .get_or_init(|| handle.block_on(start_server()))
            .url
            .clone(),
        databases: Arc::default(),
    };
    let mut cases = push_testkit::cases();
    cases.extend(push_testkit::concurrency_cases());
    let conclusion = push_testkit::run(backend, cases);

    let started = Arc::into_inner(server).and_then(OnceLock::into_inner);
    if let Some(server) = started {
        runtime
            .block_on(server.container.rm())
            .expect("remove the Postgres container");
    }
    conclusion.exit();
}
