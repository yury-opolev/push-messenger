//! Configuration from environment variables.

use std::net::SocketAddr;

use axum::http::HeaderName;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use push_credentials::{AuthMode, AzureConfig, ConfigProblem, RedisTarget, azure_for};
use push_store::{DatabaseTarget, DatabaseVars, StoreOptions, postgres_auth};

use crate::db::is_role_name;

/// How the service runs. Read from the environment by [`Config::from_env`]:
///
/// | Variable | Meaning | Default |
/// |---|---|---|
/// | `PUSH_POSTGRES_AUTH` | `password` or `azure-entra` (Microsoft Entra ID tokens, spec 2026-10-18) | `password` |
/// | `PUSH_DATABASE_URL` | password mode: Postgres URL of the `push_auth` database, as its owner | required |
/// | `PUSH_DATABASE_HOST`, `_PORT`, `_NAME`, `_USER`, `_CA_FILE` | Entra mode: the server, port (`5432`), database (`push_auth`), the principal's role and an optional CA bundle | host and user required |
/// | `PUSH_REDIS_AUTH` | `password` or `azure-entra` | `password` |
/// | `PUSH_REDIS_URL` | rate limits; password mode `redis://:password@host:6379`, Entra mode `rediss://host:port` | required |
/// | `PUSH_REDIS_USER`, `PUSH_REDIS_CA_FILE` | Entra mode: the ACL user (default: the token's `oid`) and an optional CA bundle | none |
/// | `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_FEDERATED_TOKEN_FILE`, `AZURE_AUTHORITY_HOST` | Entra mode: the identity (workload identity with the token file, else managed identity) | set by the AKS webhook |
/// | `PUSH_RUN_MIGRATIONS` | `true` or `false`: migrate and grant at start-up (else `push-auth migrate` does) | `true` |
/// | `PUSH_AUTH_PUBLIC_URL` | base URL of email links, `https://api.example.com` | required |
/// | `PUSH_AUTH_TOKEN_AUDIENCE` | the gateway's URL, the access tokens' `aud` | required |
/// | `PUSH_AUTH_SIGNING_KEY` | base64 of the 32-byte Ed25519 seed that signs access tokens | required |
/// | `PUSH_AUTH_LINK_KEY` | base64 of at least 32 bytes, the HMAC key of links and registration tokens | required |
/// | `PUSH_AUTH_CLIENT_IP_HEADER` | header with the client IP, set by the ingress (`x-real-ip`) | the peer address |
/// | `PUSH_AUTH_HASH_CONCURRENCY` | most password hashes at once (19 MiB each) | `2` |
/// | `PUSH_AUTH_HASH_QUEUE` | most requests waiting for a hash; more get a retryable 503 | `32` |
/// | `PUSH_HTTP_ADDR` | address of the API | `0.0.0.0:8080` |
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_AUTH_WORKER_ROLE` | role the worker connects as: granted the outbox and the cleanups at start-up (or by `migrate`); a plain role name, or an Entra principal name in Entra mode | none |
/// | `PUSH_MESSAGE_SVC_URL` | Directory gRPC URL of message-svc, `http://host:50053` (plain `http://` only), asked whose profiles a caller may see; without it `GET /v1/users` answers 503 | none |
/// | `PUSH_DIRECTORY_TOKEN` | the token message-svc requires for its `Directory` API, 32 to 256 visible ASCII characters; required with `PUSH_MESSAGE_SVC_URL` | none |
///
/// `http://` public URLs and `ws://` audiences are accepted only for local hosts
/// (`localhost`, `127.0.0.1`, `*.local`, `*.localhost`, `*.test`), like the chart's guard.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub database: DatabaseTarget,
    pub redis: RedisTarget,
    /// The identity, when a datastore uses Entra ID.
    pub azure: Option<AzureConfig>,
    pub run_migrations: bool,
    pub public_url: String,
    pub token_audience: String,
    pub signing_key: Vec<u8>,
    pub link_key: Vec<u8>,
    pub client_ip_header: Option<HeaderName>,
    pub hash_concurrency: usize,
    pub hash_queue: usize,
    pub http_addr: SocketAddr,
    pub health_addr: SocketAddr,
    pub store: StoreOptions,
    pub worker_role: Option<String>,
    pub message_svc_url: Option<String>,
    pub directory_token: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("database", &self.database)
            .field("redis", &self.redis)
            .field("azure", &self.azure)
            .field("run_migrations", &self.run_migrations)
            .field("public_url", &self.public_url)
            .field("token_audience", &self.token_audience)
            .field("signing_key", &"<redacted>")
            .field("link_key", &"<redacted>")
            .field("client_ip_header", &self.client_ip_header)
            .field("hash_concurrency", &self.hash_concurrency)
            .field("hash_queue", &self.hash_queue)
            .field("http_addr", &self.http_addr)
            .field("health_addr", &self.health_addr)
            .field("store", &self.store)
            .field("worker_role", &self.worker_role)
            .field("message_svc_url", &self.message_svc_url)
            .field(
                "directory_token",
                &self.directory_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} must be set")]
    Missing(&'static str),
    #[error("{name}={value:?} is not valid: {reason}")]
    Invalid {
        name: &'static str,
        value: String,
        reason: String,
    },
}

impl Config {
    /// Reads the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads variables through `lookup`, which returns a variable's value if it is set.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let required = |name: &'static str| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .ok_or(ConfigError::Missing(name))
        };
        let store = StoreOptions {
            max_connections: parsed(&lookup, "PUSH_DB_MAX_CONNECTIONS", 10)?,
            ..StoreOptions::default()
        };
        store.validate().map_err(|reason| ConfigError::Invalid {
            name: "PUSH_DB_MAX_CONNECTIONS",
            value: store.max_connections.to_string(),
            reason,
        })?;
        let lookup_dyn: &dyn Fn(&str) -> Option<String> = &lookup;
        let mode = postgres_auth(lookup_dyn).map_err(problem)?;
        let database =
            DatabaseTarget::from_lookup(lookup_dyn, mode, &DatabaseVars::own("push_auth"), None)
                .map_err(problem)?;
        let redis = RedisTarget::from_lookup(lookup_dyn).map_err(problem)?;
        let azure = azure_for(lookup_dyn, &[database.mode(), redis.mode()]).map_err(problem)?;
        let run_migrations = flag(&lookup, "PUSH_RUN_MIGRATIONS", true)?;
        let public_url = required("PUSH_AUTH_PUBLIC_URL")?;
        check_url(&public_url, "https://", "http://").map_err(|reason| ConfigError::Invalid {
            name: "PUSH_AUTH_PUBLIC_URL",
            value: public_url.clone(),
            reason,
        })?;
        let token_audience = required("PUSH_AUTH_TOKEN_AUDIENCE")?;
        check_url(&token_audience, "wss://", "ws://").map_err(|reason| ConfigError::Invalid {
            name: "PUSH_AUTH_TOKEN_AUDIENCE",
            value: token_audience.clone(),
            reason,
        })?;
        let signing_key = key(&required("PUSH_AUTH_SIGNING_KEY")?, "PUSH_AUTH_SIGNING_KEY")?;
        if signing_key.len() != 32 {
            return Err(redacted(
                "PUSH_AUTH_SIGNING_KEY",
                "must be base64 of a 32-byte Ed25519 seed".to_owned(),
            ));
        }
        let link_key = key(&required("PUSH_AUTH_LINK_KEY")?, "PUSH_AUTH_LINK_KEY")?;
        if link_key.len() < 32 {
            return Err(redacted(
                "PUSH_AUTH_LINK_KEY",
                "must be base64 of at least 32 bytes".to_owned(),
            ));
        }
        let client_ip_header =
            match lookup("PUSH_AUTH_CLIENT_IP_HEADER").filter(|v| !v.is_empty()) {
                None => None,
                Some(value) => Some(HeaderName::try_from(value.as_str()).map_err(|_| {
                    ConfigError::Invalid {
                        name: "PUSH_AUTH_CLIENT_IP_HEADER",
                        value: value.clone(),
                        reason: "not a valid header name".to_owned(),
                    }
                })?),
            };
        let hash_concurrency: usize = parsed(&lookup, "PUSH_AUTH_HASH_CONCURRENCY", 2)?;
        if hash_concurrency == 0 {
            return Err(ConfigError::Invalid {
                name: "PUSH_AUTH_HASH_CONCURRENCY",
                value: "0".to_owned(),
                reason: "must be at least 1".to_owned(),
            });
        }
        let worker_role = worker_role(&lookup, mode)?;
        let message_svc_url = lookup("PUSH_MESSAGE_SVC_URL").filter(|v| !v.is_empty());
        if let Some(url) = &message_svc_url {
            if !url.starts_with("http://") {
                return Err(ConfigError::Invalid {
                    name: "PUSH_MESSAGE_SVC_URL",
                    value: url.clone(),
                    reason: "must start with http:// (auth-svc speaks plain gRPC to \
                             message-svc inside the cluster and is built without TLS)"
                        .to_owned(),
                });
            }
        }
        let directory_token = lookup("PUSH_DIRECTORY_TOKEN").filter(|v| !v.is_empty());
        match (&message_svc_url, &directory_token) {
            (Some(_), None) => return Err(ConfigError::Missing("PUSH_DIRECTORY_TOKEN")),
            (None, Some(_)) => return Err(ConfigError::Missing("PUSH_MESSAGE_SVC_URL")),
            _ => {}
        }
        if let Some(token) = &directory_token {
            if !((32..=256).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_graphic())) {
                return Err(redacted(
                    "PUSH_DIRECTORY_TOKEN",
                    "use 32 to 256 visible ASCII characters".to_owned(),
                ));
            }
        }
        Ok(Self {
            database,
            redis,
            azure,
            run_migrations,
            public_url,
            token_audience,
            signing_key,
            link_key,
            client_ip_header,
            hash_concurrency,
            hash_queue: parsed(&lookup, "PUSH_AUTH_HASH_QUEUE", 32)?,
            http_addr: parsed(&lookup, "PUSH_HTTP_ADDR", ([0, 0, 0, 0], 8080).into())?,
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            store,
            worker_role,
            message_svc_url,
            directory_token,
        })
    }
}

/// A key variable, base64-decoded; the value is never shown.
fn key(value: &str, name: &'static str) -> Result<Vec<u8>, ConfigError> {
    STANDARD
        .decode(value.trim())
        .map_err(|_| redacted(name, "not valid base64".to_owned()))
}

/// Checks a public URL: `secure` scheme, or `local` only for a local host.
fn check_url(url: &str, secure: &str, local: &str) -> Result<(), String> {
    // No userinfo or backslashes: either could make the host check read the wrong host.
    if url.contains(['@', '\\']) {
        return Err("must be a plain URL with a host, without user info".to_owned());
    }
    let rest = if let Some(rest) = url.strip_prefix(secure) {
        rest
    } else if let Some(rest) = url.strip_prefix(local) {
        if !is_local_host(host_of(rest)) {
            return Err(format!(
                "{local} is only for local hosts (localhost, *.local, *.localhost, *.test); use {secure}"
            ));
        }
        rest
    } else {
        return Err(format!("must start with {secure}"));
    };
    let host = host_of(rest);
    if host.is_empty() || url.contains(['?', '#', ' ']) {
        return Err("must be a plain URL with a host, without query or fragment".to_owned());
    }
    Ok(())
}

/// The host of `rest`, the URL after its scheme: up to the port or path.
fn host_of(rest: &str) -> &str {
    let authority = rest.split('/').next().unwrap_or_default();
    match authority.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => authority,
    }
}

fn is_local_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
        || [".local", ".localhost", ".test"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
}

/// An invalid secret variable; the value is never shown.
/// What `push-auth migrate` reads: the database variables, the identity,
/// `PUSH_DATABASE_RUNTIME_ROLE` (granted the use of the `auth` schema) and
/// `PUSH_AUTH_WORKER_ROLE` (granted the worker's narrow access, as at start-up).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateConfig {
    pub database: DatabaseTarget,
    pub azure: Option<AzureConfig>,
    pub runtime_role: Option<String>,
    pub worker_role: Option<String>,
}

impl MigrateConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let lookup_dyn: &dyn Fn(&str) -> Option<String> = &lookup;
        let mode = postgres_auth(lookup_dyn).map_err(problem)?;
        let database =
            DatabaseTarget::from_lookup(lookup_dyn, mode, &DatabaseVars::own("push_auth"), None)
                .map_err(problem)?;
        let azure = azure_for(lookup_dyn, &[database.mode()]).map_err(problem)?;
        Ok(Self {
            database,
            azure,
            runtime_role: runtime_role(&lookup)?,
            worker_role: worker_role(&lookup, mode)?,
        })
    }
}

/// `PUSH_AUTH_WORKER_ROLE`: a plain role name in password mode (as before), an Entra
/// principal name in Entra mode; unset is `None`.
fn worker_role(
    lookup: &impl Fn(&str) -> Option<String>,
    mode: AuthMode,
) -> Result<Option<String>, ConfigError> {
    let Some(role) = lookup("PUSH_AUTH_WORKER_ROLE").filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let (valid, reason) = match mode {
        AuthMode::Password => (
            is_role_name(&role),
            "a role name: lower-case letters, digits and _, not starting with a digit",
        ),
        AuthMode::AzureEntra => (
            push_store::is_principal_name(&role),
            "an Entra principal name: 1 to 63 ASCII letters, digits, _ - . or @",
        ),
    };
    if valid {
        Ok(Some(role))
    } else {
        Err(ConfigError::Invalid {
            name: "PUSH_AUTH_WORKER_ROLE",
            value: role,
            reason: reason.to_owned(),
        })
    }
}

/// `PUSH_DATABASE_RUNTIME_ROLE`: a principal name, or unset.
fn runtime_role(lookup: &impl Fn(&str) -> Option<String>) -> Result<Option<String>, ConfigError> {
    match lookup("PUSH_DATABASE_RUNTIME_ROLE").filter(|role| !role.trim().is_empty()) {
        None => Ok(None),
        Some(role) if push_store::is_principal_name(role.trim()) => {
            Ok(Some(role.trim().to_owned()))
        }
        Some(role) => Err(ConfigError::Invalid {
            name: "PUSH_DATABASE_RUNTIME_ROLE",
            value: role,
            reason: "expected 1 to 63 ASCII letters, digits, _ - . or @".to_owned(),
        }),
    }
}

/// A datastore or identity variable that is missing or invalid; values are never shown.
fn problem(problem: ConfigProblem) -> ConfigError {
    match problem {
        ConfigProblem::Missing(name) => ConfigError::Missing(name),
        ConfigProblem::Invalid { name, reason } => redacted(name, reason),
    }
}

/// `true` or `false`; `default` when unset or empty.
fn flag(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: bool,
) -> Result<bool, ConfigError> {
    match lookup(name).as_deref().map(str::trim) {
        None | Some("") => Ok(default),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(other) => Err(ConfigError::Invalid {
            name,
            value: other.to_owned(),
            reason: "expected true or false".to_owned(),
        }),
    }
}

fn redacted(name: &'static str, reason: String) -> ConfigError {
    ConfigError::Invalid {
        name,
        value: "<redacted>".to_owned(),
        reason,
    }
}

/// A variable parsed as `T`, or `default` when it is unset or empty.
fn parsed<T>(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: T,
) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match lookup(name).filter(|value| !value.is_empty()) {
        None => Ok(default),
        Some(value) => value.parse().map_err(|error: T::Err| ConfigError::Invalid {
            name,
            reason: error.to_string(),
            value,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config(variables: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let variables: HashMap<String, String> = variables
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        Config::from_lookup(|name| variables.get(name).cloned())
    }

    /// base64 of 32 bytes of 0x07 and of 0x09.
    const SIGNING_KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";
    const LINK_KEY: &str = "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=";

    const REQUIRED: [(&str, &str); 6] = [
        ("PUSH_DATABASE_URL", "postgres://user:hunter2@db/push_auth"),
        ("PUSH_REDIS_URL", "redis://:hunter2@redis:6379"),
        ("PUSH_AUTH_PUBLIC_URL", "https://api.example.com"),
        (
            "PUSH_AUTH_TOKEN_AUDIENCE",
            "wss://api.example.com/v1/socket",
        ),
        ("PUSH_AUTH_SIGNING_KEY", SIGNING_KEY),
        ("PUSH_AUTH_LINK_KEY", LINK_KEY),
    ];

    fn with(name: &'static str, value: &'static str) -> Vec<(&'static str, &'static str)> {
        let mut variables = REQUIRED.to_vec();
        variables.retain(|(key, _)| *key != name);
        variables.push((name, value));
        variables
    }

    #[test]
    fn defaults_apply_when_only_the_required_variables_are_set() {
        let config = config(&REQUIRED).unwrap();
        assert_eq!(config.signing_key, [7; 32]);
        assert_eq!(config.link_key, [9; 32]);
        assert_eq!(config.http_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.hash_concurrency, 2);
        assert_eq!(config.hash_queue, 32);
        assert_eq!(config.client_ip_header, None);
        assert_eq!(config.store, StoreOptions::default());
        assert_eq!(config.worker_role, None);
        assert_eq!(config.message_svc_url, None);
        assert_eq!(config.directory_token, None);
    }

    #[test]
    fn every_required_variable_is_required() {
        for (index, (name, _)) in REQUIRED.iter().enumerate() {
            let mut variables = REQUIRED.to_vec();
            variables.remove(index);
            assert_eq!(config(&variables), Err(ConfigError::Missing(name)));
        }
    }

    #[test]
    fn optional_settings_can_be_set() {
        let mut variables = with("PUSH_AUTH_PUBLIC_URL", "https://push.test");
        variables.extend([
            ("PUSH_AUTH_CLIENT_IP_HEADER", "X-Real-IP"),
            ("PUSH_AUTH_HASH_CONCURRENCY", "4"),
            ("PUSH_AUTH_HASH_QUEUE", "8"),
            ("PUSH_HTTP_ADDR", "127.0.0.1:7000"),
            ("PUSH_DB_MAX_CONNECTIONS", "3"),
            ("PUSH_AUTH_WORKER_ROLE", "push_worker"),
        ]);
        let config = config(&variables).unwrap();
        assert_eq!(
            config.client_ip_header,
            Some(HeaderName::from_static("x-real-ip"))
        );
        assert_eq!(config.hash_concurrency, 4);
        assert_eq!(config.hash_queue, 8);
        assert_eq!(config.http_addr, "127.0.0.1:7000".parse().unwrap());
        assert_eq!(config.store.max_connections, 3);
        assert_eq!(config.worker_role.as_deref(), Some("push_worker"));
    }

    #[test]
    fn plain_http_and_ws_are_for_local_hosts_only() {
        let mut variables = with("PUSH_AUTH_PUBLIC_URL", "http://push.local");
        variables.retain(|(name, _)| *name != "PUSH_AUTH_TOKEN_AUDIENCE");
        variables.push(("PUSH_AUTH_TOKEN_AUDIENCE", "ws://push.local:8080/v1/socket"));
        let local = config(&variables);
        assert!(local.is_ok(), "{local:?}");
        assert!(config(&with("PUSH_AUTH_PUBLIC_URL", "http://localhost:8080")).is_ok());
        for (name, value) in [
            ("PUSH_AUTH_PUBLIC_URL", "http://api.example.com"),
            ("PUSH_AUTH_PUBLIC_URL", "ftp://api.example.com"),
            ("PUSH_AUTH_PUBLIC_URL", "https://api.example.com/?x=1"),
            ("PUSH_AUTH_PUBLIC_URL", "https://"),
            ("PUSH_AUTH_PUBLIC_URL", "https://evil.com\\.test"),
            ("PUSH_AUTH_PUBLIC_URL", "https://user@evil.com"),
            ("PUSH_AUTH_PUBLIC_URL", "http://evil.com@push.test"),
            ("PUSH_AUTH_TOKEN_AUDIENCE", "ws://api.example.com/v1/socket"),
            (
                "PUSH_AUTH_TOKEN_AUDIENCE",
                "https://api.example.com/v1/socket",
            ),
        ] {
            assert!(
                matches!(config(&with(name, value)), Err(ConfigError::Invalid { name: n, .. }) if n == name),
                "{name}={value}"
            );
        }
    }

    #[test]
    fn bad_keys_fail_start_up_without_showing_them() {
        for (name, value) in [
            ("PUSH_AUTH_SIGNING_KEY", "not base64!"),
            ("PUSH_AUTH_SIGNING_KEY", "AAAA"),
            (
                "PUSH_AUTH_LINK_KEY",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            ),
        ] {
            let message = config(&with(name, value)).unwrap_err().to_string();
            assert!(message.contains(name), "{message}");
            assert!(!message.contains(value), "{message}");
        }
    }

    #[test]
    fn urls_and_keys_never_show_in_errors_or_debug_output() {
        let message = config(&with("PUSH_DATABASE_URL", "mysql://user:hunter2@db/x"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("PUSH_DATABASE_URL") && !message.contains("hunter2"));
        let message = config(&with("PUSH_REDIS_URL", "http://:hunter2@redis:6379"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("PUSH_REDIS_URL") && !message.contains("hunter2"));
        let shown = format!("{:?}", config(&REQUIRED).unwrap());
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(!shown.contains("[7, 7"), "{shown}");
        assert!(shown.contains("https://api.example.com"), "{shown}");
    }

    #[test]
    fn entra_mode_needs_the_parts_and_accepts_principal_names() {
        let mut variables = REQUIRED.to_vec();
        variables.retain(|(name, _)| *name != "PUSH_DATABASE_URL");
        variables.extend([
            ("PUSH_POSTGRES_AUTH", "azure-entra"),
            ("PUSH_DATABASE_HOST", "pg.example.net"),
            ("PUSH_DATABASE_USER", "push-auth-id"),
            ("PUSH_AUTH_WORKER_ROLE", "push-worker-id"),
        ]);
        let entra = config(&variables).unwrap();
        assert!(
            matches!(entra.database, DatabaseTarget::Entra(ref database) if database.name == "push_auth")
        );
        assert_eq!(entra.worker_role.as_deref(), Some("push-worker-id"));
        assert!(
            entra.azure.is_some(),
            "the identity is read (managed identity here)"
        );
        // Password mode keeps the plain role names it always accepted.
        assert!(matches!(
            config(&with("PUSH_AUTH_WORKER_ROLE", "push-worker-id")),
            Err(ConfigError::Invalid {
                name: "PUSH_AUTH_WORKER_ROLE",
                ..
            })
        ));
    }

    #[test]
    fn the_worker_role_must_be_a_plain_role_name() {
        for role in ["Push", "push-worker", "w\"; drop", "1w"] {
            assert!(
                matches!(
                    config(&with("PUSH_AUTH_WORKER_ROLE", role)),
                    Err(ConfigError::Invalid {
                        name: "PUSH_AUTH_WORKER_ROLE",
                        ..
                    })
                ),
                "{role}"
            );
        }
    }

    #[test]
    fn message_svc_comes_with_its_directory_token_and_plain_http() {
        let token = "d1rectory-d1rectory-d1rectory-d1rectory-";
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_MESSAGE_SVC_URL", "http://message-svc:50051"));
        variables.push(("PUSH_DIRECTORY_TOKEN", token));
        let configured = config(&variables).unwrap();
        assert_eq!(
            configured.message_svc_url.as_deref(),
            Some("http://message-svc:50051")
        );
        assert_eq!(configured.directory_token.as_deref(), Some(token));
        assert!(!format!("{configured:?}").contains(token));

        assert_eq!(
            config(&with("PUSH_MESSAGE_SVC_URL", "http://message-svc:50051")),
            Err(ConfigError::Missing("PUSH_DIRECTORY_TOKEN"))
        );
        assert_eq!(
            config(&with("PUSH_DIRECTORY_TOKEN", token)),
            Err(ConfigError::Missing("PUSH_MESSAGE_SVC_URL"))
        );
        let mut tls = variables.clone();
        tls[REQUIRED.len()] = ("PUSH_MESSAGE_SVC_URL", "https://message-svc:50051");
        assert!(matches!(
            config(&tls),
            Err(ConfigError::Invalid {
                name: "PUSH_MESSAGE_SVC_URL",
                ..
            })
        ));
        let mut short = variables;
        short[REQUIRED.len() + 1] = ("PUSH_DIRECTORY_TOKEN", "too-short");
        let message = config(&short).unwrap_err().to_string();
        assert!(message.contains("PUSH_DIRECTORY_TOKEN"), "{message}");
        assert!(!message.contains("too-short"), "{message}");
    }

    #[test]
    fn zero_hash_concurrency_is_refused() {
        assert!(config(&with("PUSH_AUTH_HASH_CONCURRENCY", "0")).is_err());
    }
}
