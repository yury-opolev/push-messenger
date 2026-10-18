//! Test doubles for Entra ID credentials (feature `testing`): everything of
//! [`crate::stub`], plus a test certificate authority and PostgreSQL and Redis containers
//! that require TLS. Tests and examples only (testcontainers and rcgen's `time` need Rust
//! 1.88); never used by a binary.

use std::path::{Path as FilePath, PathBuf};
use std::time::Duration;

use rcgen::{BasicConstraints, Certificate, CertificateParams, DnType, IsCa, KeyPair};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use testcontainers_modules::testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, GenericImage, ImageExt};

pub use crate::stub::*;

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
        self.server_for(&["127.0.0.1", "localhost"])
    }

    /// A server certificate for `names` (DNS names or IP addresses): (certificate, key),
    /// PEM.
    pub fn server_for(&self, names: &[&str]) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        // rcgen turns "127.0.0.1" into an IP address name.
        let names = names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>();
        let params = CertificateParams::new(names).unwrap();
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
