//! How a service reaches its PostgreSQL database (spec 2026-10-18 §5): a password URL, or
//! an Entra ID principal whose access token is the password. In Entra mode TLS is always
//! verified (`verify-full`, the system roots or `ca_file`), new connections take the
//! newest token, and pooled connections live at most 30 minutes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use push_credentials::{AccessToken, AuthMode, ConfigProblem, TokenError, TokenSource, Tokens};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::StoreOptions;

/// The scope of Azure Database for PostgreSQL tokens.
pub const POSTGRES_SCOPE: &str = "https://ossrdbms-aad.database.windows.net/.default";
const ENTRA_MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// Where a database is and how to sign in. `Debug` never shows a password URL.
#[derive(Clone, PartialEq, Eq)]
pub enum DatabaseTarget {
    Url(String),
    Entra(EntraDatabase),
}

impl std::fmt::Debug for DatabaseTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Url(_) => formatter.write_str("Url(<redacted>)"),
            Self::Entra(database) => formatter.debug_tuple("Entra").field(database).finish(),
        }
    }
}

/// A database reached with an Entra principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntraDatabase {
    pub host: String,
    pub port: u16,
    pub name: String,
    /// The principal's PostgreSQL role ([`is_principal_name`]).
    pub user: String,
    pub ca_file: Option<PathBuf>,
}

/// The variable names of one database connection.
#[derive(Debug, Clone, Copy)]
pub struct DatabaseVars {
    pub url: &'static str,
    pub host: &'static str,
    pub port: &'static str,
    pub name: &'static str,
    pub user: &'static str,
    pub ca_file: &'static str,
    pub default_name: &'static str,
}

impl DatabaseVars {
    /// A service's own database: `PUSH_DATABASE_*`.
    pub const fn own(default_name: &'static str) -> Self {
        Self {
            url: "PUSH_DATABASE_URL",
            host: "PUSH_DATABASE_HOST",
            port: "PUSH_DATABASE_PORT",
            name: "PUSH_DATABASE_NAME",
            user: "PUSH_DATABASE_USER",
            ca_file: "PUSH_DATABASE_CA_FILE",
            default_name,
        }
    }

    /// The worker's connection to auth-svc's database: `PUSH_AUTH_DATABASE_*`.
    pub const AUTH_FOR_WORKER: Self = Self {
        url: "PUSH_AUTH_DATABASE_URL",
        host: "PUSH_AUTH_DATABASE_HOST",
        port: "PUSH_AUTH_DATABASE_PORT",
        name: "PUSH_AUTH_DATABASE_NAME",
        user: "PUSH_AUTH_DATABASE_USER",
        ca_file: "PUSH_AUTH_DATABASE_CA_FILE",
        default_name: "push_auth",
    };
}

/// `PUSH_POSTGRES_AUTH`.
pub fn postgres_auth(lookup: &dyn Fn(&str) -> Option<String>) -> Result<AuthMode, ConfigProblem> {
    AuthMode::parse(lookup("PUSH_POSTGRES_AUTH").as_deref()).map_err(|reason| {
        ConfigProblem::Invalid {
            name: "PUSH_POSTGRES_AUTH",
            reason,
        }
    })
}

impl DatabaseTarget {
    /// Reads one database's variables. In Entra mode host, port, user and CA file default
    /// to `fallback`'s (the worker's own database, for its second connection).
    pub fn from_lookup(
        lookup: &dyn Fn(&str) -> Option<String>,
        mode: AuthMode,
        vars: &DatabaseVars,
        fallback: Option<&EntraDatabase>,
    ) -> Result<Self, ConfigProblem> {
        let get = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        if mode == AuthMode::Password {
            let url = get(vars.url).ok_or(ConfigProblem::Missing(vars.url))?;
            crate::check_database_url(&url).map_err(|reason| ConfigProblem::Invalid {
                name: vars.url,
                reason,
            })?;
            return Ok(Self::Url(url));
        }
        if get(vars.url).is_some() {
            return Err(ConfigProblem::Invalid {
                name: vars.url,
                reason: "not used with PUSH_POSTGRES_AUTH=azure-entra: unset it and set the \
                         host, name and user"
                    .to_owned(),
            });
        }
        let host = get(vars.host)
            .or_else(|| fallback.map(|f| f.host.clone()))
            .ok_or(ConfigProblem::Missing(vars.host))?;
        let port = match get(vars.port) {
            Some(port) => port
                .parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| ConfigProblem::Invalid {
                    name: vars.port,
                    reason: "expected a port number".to_owned(),
                })?,
            None => fallback.map_or(5432, |f| f.port),
        };
        let name = get(vars.name).unwrap_or_else(|| vars.default_name.to_owned());
        if !is_principal_name(&name) {
            return Err(ConfigProblem::Invalid {
                name: vars.name,
                reason: "not a database name".to_owned(),
            });
        }
        let user = get(vars.user)
            .or_else(|| fallback.map(|f| f.user.clone()))
            .ok_or(ConfigProblem::Missing(vars.user))?;
        if !is_principal_name(&user) {
            return Err(ConfigProblem::Invalid {
                name: vars.user,
                reason: "expected 1 to 63 ASCII letters, digits, _ - . or @".to_owned(),
            });
        }
        let ca_file = get(vars.ca_file)
            .map(PathBuf::from)
            .or_else(|| fallback.and_then(|f| f.ca_file.clone()));
        Ok(Self::Entra(EntraDatabase {
            host,
            port,
            name,
            user,
            ca_file,
        }))
    }

    pub fn mode(&self) -> AuthMode {
        match self {
            Self::Url(_) => AuthMode::Password,
            Self::Entra(_) => AuthMode::AzureEntra,
        }
    }
}

/// The PostgreSQL tokens a target needs: `None` for a password URL; in Entra mode the
/// first token from `azure` (failing when none can be had), refreshed until `shutdown`.
pub async fn tokens_for(
    target: &DatabaseTarget,
    azure: Option<&Arc<dyn TokenSource>>,
    shutdown: CancellationToken,
) -> Result<Option<Tokens>, TokenError> {
    match target {
        DatabaseTarget::Url(_) => Ok(None),
        DatabaseTarget::Entra(_) => {
            let source = azure.ok_or_else(|| {
                TokenError::Config(
                    "PUSH_POSTGRES_AUTH=azure-entra needs the AZURE_* identity variables"
                        .to_owned(),
                )
            })?;
            Tokens::start(Arc::clone(source), POSTGRES_SCOPE, shutdown)
                .await
                .map(Some)
        }
    }
}

/// Opens pools and migration connections for one database. Cheap to clone.
#[derive(Clone)]
pub struct Connector {
    base: PgConnectOptions,
    tokens: Option<Tokens>,
}

impl Connector {
    pub fn from_url(url: &str) -> Result<Self, sqlx::Error> {
        Ok(Self {
            base: url.parse()?,
            tokens: None,
        })
    }

    /// Fixed options (a password role): tests and tools.
    pub fn from_options(options: PgConnectOptions) -> Self {
        Self {
            base: options,
            tokens: None,
        }
    }

    /// `database` signed in with `tokens` (scope [`POSTGRES_SCOPE`]).
    pub fn entra(database: &EntraDatabase, tokens: Tokens) -> Self {
        let mut base = PgConnectOptions::new_without_pgpass()
            .host(&database.host)
            .port(database.port)
            .database(&database.name)
            .username(&database.user)
            .ssl_mode(PgSslMode::VerifyFull);
        if let Some(ca_file) = &database.ca_file {
            base = base.ssl_root_cert(ca_file);
        }
        Self {
            base,
            tokens: Some(tokens),
        }
    }

    /// The connector for `target`; Entra targets need `tokens`.
    pub fn for_target(
        target: &DatabaseTarget,
        tokens: Option<&Tokens>,
    ) -> Result<Self, sqlx::Error> {
        match (target, tokens) {
            (DatabaseTarget::Url(url), _) => Self::from_url(url),
            (DatabaseTarget::Entra(database), Some(tokens)) => {
                Ok(Self::entra(database, tokens.clone()))
            }
            (DatabaseTarget::Entra(_), None) => Err(sqlx::Error::Configuration(
                "PUSH_POSTGRES_AUTH=azure-entra needs an access token".into(),
            )),
        }
    }

    pub fn is_entra(&self) -> bool {
        self.tokens.is_some()
    }

    /// The options of a new connection: in Entra mode the newest token is the password.
    pub fn options(&self) -> PgConnectOptions {
        match &self.tokens {
            Some(tokens) => self.base.clone().password(tokens.current().secret()),
            None => self.base.clone(),
        }
    }

    /// A request pool as [`crate::connect_pool`] describes; in Entra mode its connect
    /// options follow every new token until the pool closes.
    pub async fn pool(&self, store: &StoreOptions) -> Result<PgPool, sqlx::Error> {
        use sqlx::Executor;

        store
            .validate()
            .map_err(|message| sqlx::Error::Configuration(message.into()))?;
        let settings = store.session_settings();
        let mut options = PgPoolOptions::new()
            .max_connections(store.max_connections)
            .acquire_timeout(store.acquire_timeout)
            .after_connect(move |connection, _| {
                let settings = settings.clone();
                Box::pin(async move {
                    connection.execute(settings.as_str()).await?;
                    Ok(())
                })
            });
        if self.is_entra() {
            options = options.max_lifetime(ENTRA_MAX_LIFETIME);
        }
        let Some(tokens) = &self.tokens else {
            return options.connect_with(self.options()).await;
        };
        // The receiver exists before the first connect and marks the token used as seen:
        // a token sent while connecting is then still pending for `rotate`.
        let mut receiver = tokens.watch();
        let token = receiver.borrow_and_update().clone();
        let pool = options
            .connect_with(self.base.clone().password(token.secret()))
            .await?;
        tokio::spawn(rotate(pool.clone(), self.base.clone(), receiver));
        Ok(pool)
    }

    /// One connection without the request timeouts, for migrations and grants.
    pub async fn single(&self) -> Result<PgPool, sqlx::Error> {
        PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(self.options())
            .await
    }

    /// Applies `migrator`'s pending migrations on a connection of its own, after checking
    /// that each of `extensions` is allow-listed where the server has an allow-list
    /// (Azure's `azure.extensions`).
    pub async fn migrate(
        &self,
        migrator: &sqlx::migrate::Migrator,
        extensions: &[&str],
    ) -> Result<(), sqlx::migrate::MigrateError> {
        let pool = self.single().await?;
        let result = async {
            for extension in extensions {
                check_allow_listed(&pool, extension).await?;
            }
            migrator.run(&pool).await
        }
        .await;
        pool.close().await;
        result
    }
}

async fn rotate(pool: PgPool, base: PgConnectOptions, mut tokens: watch::Receiver<AccessToken>) {
    loop {
        tokio::select! {
            () = pool.close_event() => return,
            changed = tokens.changed() => if changed.is_err() { return },
        }
        let token = tokens.borrow_and_update().clone();
        pool.set_connect_options(base.clone().password(token.secret()));
    }
}

/// Fails with a clear message when the server allow-lists extensions (Azure:
/// `azure.extensions`) and `extension` is not on the list.
pub async fn check_allow_listed(pool: &PgPool, extension: &str) -> Result<(), sqlx::Error> {
    let list: Option<String> =
        sqlx::query_scalar("SELECT current_setting('azure.extensions', true)")
            .fetch_one(pool)
            .await?;
    match list {
        Some(list) if !allow_listed(&list, extension) => Err(sqlx::Error::Configuration(
            format!(
                "the {extension} extension is not allow-listed: add {} to the server parameter \
                 azure.extensions",
                extension.to_ascii_uppercase()
            )
            .into(),
        )),
        _ => Ok(()),
    }
}

fn allow_listed(list: &str, extension: &str) -> bool {
    list.split(',')
        .any(|entry| entry.trim().eq_ignore_ascii_case(extension))
}

/// Grants `role` the use of everything in `schema` (`USAGE`; `SELECT, INSERT, UPDATE,
/// DELETE` on its tables; `USAGE, SELECT, UPDATE` on its sequences; `EXECUTE` on its
/// functions), for a runtime identity that does not own the schema. Run it as the owner
/// after every migration. Idempotent.
pub async fn grant_runtime_access(
    pool: &PgPool,
    schema: &str,
    role: &str,
) -> Result<(), sqlx::Error> {
    if !is_principal_name(role) || !schema.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        return Err(sqlx::Error::Configuration(
            "not a valid role or schema name".into(),
        ));
    }
    let grants = format!(
        "GRANT USAGE ON SCHEMA {schema} TO \"{role}\"; \
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA {schema} TO \"{role}\"; \
         GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA {schema} TO \"{role}\"; \
         GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA {schema} TO \"{role}\""
    );
    sqlx::raw_sql(&grants).execute(pool).await.map(drop)
}

/// A role name safe to quote into SQL: 1 to 63 bytes of ASCII letters, digits, `_`, `-`,
/// `.` or `@` (Entra principals: managed identity names, groups, UPNs), and none of
/// `public`, `none` or `pg_*`, which PostgreSQL reserves.
pub fn is_principal_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@'))
        && !matches!(lower.as_str(), "public" | "none")
        && !lower.starts_with("pg_")
}

/// Whether a connection or migration error is configuration rather than an outage: a
/// refused password or token (SQLSTATE 28P01, 28000), a rejected server certificate, or a
/// configuration error such as a missing allow-listed extension. Other TLS failures (a
/// handshake cut short) stay retryable. Services stop on these in Entra mode.
pub fn is_permanent(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Configuration(_) => true,
        sqlx::Error::Tls(inner) => push_credentials::is_certificate_error(inner.as_ref()),
        // sqlx reports a rejected certificate during the handshake as an I/O error.
        sqlx::Error::Io(inner) => push_credentials::is_certificate_error(inner),
        error => error
            .as_database_error()
            .and_then(|error| error.code())
            .is_some_and(|code| code == "28P01" || code == "28000"),
    }
}

/// [`is_permanent`] anywhere in `error`'s chain, also inside a migration error; a
/// migration refused for lack of privilege (42501) is permanent too: the migrating
/// identity is not the database's owner.
pub fn is_permanent_startup(error: &anyhow::Error) -> bool {
    use sqlx::migrate::MigrateError;
    let in_migration = |error: &sqlx::Error| {
        is_permanent(error)
            || error
                .as_database_error()
                .and_then(|error| error.code())
                .is_some_and(|code| code == "42501")
    };
    error.chain().any(|cause| {
        cause
            .downcast_ref::<sqlx::Error>()
            .is_some_and(is_permanent)
            || match cause.downcast_ref::<MigrateError>() {
                Some(MigrateError::Execute(error))
                | Some(MigrateError::ExecuteMigration(error, _)) => in_migration(error),
                _ => false,
            }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn password_mode_reads_the_url_and_entra_mode_the_parts() {
        let own = DatabaseVars::own("push_messaging");
        let url = DatabaseTarget::from_lookup(
            &lookup(&[("PUSH_DATABASE_URL", "postgres://u:hunter2@db/x")]),
            AuthMode::Password,
            &own,
            None,
        )
        .unwrap();
        assert_eq!(format!("{url:?}"), "Url(<redacted>)");
        let entra = DatabaseTarget::from_lookup(
            &lookup(&[
                ("PUSH_DATABASE_HOST", "pg.example.net"),
                ("PUSH_DATABASE_USER", "push-message-svc-id"),
            ]),
            AuthMode::AzureEntra,
            &own,
            None,
        )
        .unwrap();
        assert_eq!(
            entra,
            DatabaseTarget::Entra(EntraDatabase {
                host: "pg.example.net".into(),
                port: 5432,
                name: "push_messaging".into(),
                user: "push-message-svc-id".into(),
                ca_file: None,
            })
        );
        assert_eq!(
            DatabaseTarget::from_lookup(
                &lookup(&[("PUSH_DATABASE_HOST", "h")]),
                AuthMode::AzureEntra,
                &own,
                None
            ),
            Err(ConfigProblem::Missing("PUSH_DATABASE_USER"))
        );
        assert!(matches!(
            DatabaseTarget::from_lookup(
                &lookup(&[("PUSH_DATABASE_URL", "postgres://u:hunter2@db/x")]),
                AuthMode::AzureEntra,
                &own,
                None
            ),
            Err(ConfigProblem::Invalid {
                name: "PUSH_DATABASE_URL",
                ..
            })
        ));
    }

    #[test]
    fn the_workers_auth_database_defaults_to_its_own_server_and_principal() {
        let own = EntraDatabase {
            host: "pg".into(),
            port: 6432,
            name: "push_worker".into(),
            user: "push-worker-id".into(),
            ca_file: None,
        };
        let auth = DatabaseTarget::from_lookup(
            &lookup(&[]),
            AuthMode::AzureEntra,
            &DatabaseVars::AUTH_FOR_WORKER,
            Some(&own),
        )
        .unwrap();
        assert_eq!(
            auth,
            DatabaseTarget::Entra(EntraDatabase {
                name: "push_auth".into(),
                ..own
            })
        );
    }

    #[test]
    fn principal_names_allow_entra_characters_only() {
        for name in ["push_worker", "push-auth-id", "Push.App@contoso.com", "a"] {
            assert!(is_principal_name(name), "{name}");
        }
        let long = "x".repeat(64);
        for name in [
            "",
            "public",
            "PUBLIC",
            "none",
            "pg_monitor",
            "a\"b",
            "a b",
            "a;b",
            long.as_str(),
        ] {
            assert!(!is_principal_name(name), "{name}");
        }
    }

    #[test]
    fn the_allow_list_is_comma_separated_and_case_insensitive() {
        assert!(allow_listed("PG_TRGM, CITEXT", "citext"));
        assert!(!allow_listed("PG_TRGM", "citext"));
        assert!(!allow_listed("", "citext"));
    }

    #[test]
    fn only_rejected_certificates_make_a_tls_error_permanent() {
        let rejected = sqlx::Error::Tls(Box::new(rustls_error()));
        assert!(is_permanent(&rejected));
        let reset = sqlx::Error::Tls(Box::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        )));
        assert!(!is_permanent(&reset));
        assert!(is_permanent(&sqlx::Error::Io(rustls_error())));
        let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert!(!is_permanent(&sqlx::Error::Io(refused)));
        assert!(!is_permanent(&sqlx::Error::PoolTimedOut));
    }

    fn rustls_error() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        )
    }
}
