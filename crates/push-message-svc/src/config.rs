//! Configuration from environment variables.
//!
//! Internal API tokens also accept `_FILE` alternatives, read once at startup.

use std::net::SocketAddr;
use std::time::Duration;

use push_credentials::{AzureConfig, ConfigProblem, RedisTarget, azure_for};
use push_store::{DatabaseTarget, DatabaseVars, StoreOptions, postgres_auth};

/// How the service runs. Read from the environment by [`Config::from_env`]:
///
/// | Variable | Meaning | Default |
/// |---|---|---|
/// | `PUSH_POSTGRES_AUTH` | `password` or `azure-entra` (Microsoft Entra ID tokens, spec 2026-10-18) | `password` |
/// | `PUSH_DATABASE_URL` | password mode: Postgres URL of the `push_messaging` database, as its owner | required |
/// | `PUSH_DATABASE_HOST`, `_PORT`, `_NAME`, `_USER`, `_CA_FILE` | Entra mode: the server, port (`5432`), database (`push_messaging`), the principal's role and an optional CA bundle | host and user required |
/// | `PUSH_REDIS_AUTH` | `password` or `azure-entra` | `password` |
/// | `PUSH_REDIS_URL` | password mode `redis://:password@host:6379`; Entra mode `rediss://host:port` | required |
/// | `PUSH_REDIS_USER`, `PUSH_REDIS_CA_FILE` | Entra mode: the ACL user (default: the token's `oid`) and an optional CA bundle | none |
/// | `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_FEDERATED_TOKEN_FILE`, `AZURE_AUTHORITY_HOST` | Entra mode: the identity (workload identity with the token file, else managed identity) | set by the AKS webhook |
/// | `PUSH_RUN_MIGRATIONS` | `true` or `false`: migrate at start-up (else `push-message-svc migrate` does) | `true` |
/// | `PUSH_GRPC_ADDR` | address of the `Messaging` gRPC API | `0.0.0.0:50051` |
/// | `PUSH_ACCOUNTS_GRPC_ADDR` | address of the `Accounts` gRPC API | `0.0.0.0:50052` |
/// | `PUSH_DIRECTORY_GRPC_ADDR` | address of the `Directory` gRPC API | `0.0.0.0:50053` |
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_ACCOUNTS_TOKEN` | the worker's token for the `Accounts` API, 32 to 256 visible ASCII characters; without it `Accounts` refuses every call | none |
/// | `PUSH_DIRECTORY_TOKEN` | auth-svc's token for the `Directory` API, 32 to 256 visible ASCII characters and different from `PUSH_ACCOUNTS_TOKEN`; without it `Directory` refuses every call | none |
/// | `PUSH_MESSAGING_TOKEN` | the gateway's token for the `Messaging` API, 32 to 256 visible ASCII characters and different from the other two; without it `Messaging` refuses every call | none |
/// | `PUSH_CALLS_ENABLED` | `1` or `0`: run the call engine (docs/calls.md); enable once every gateway and message-svc replica has calls | `0` |
/// | `PUSH_CALLS_RING_SECONDS` | how long a call rings, 10 to 120 | `45` |
/// | `PUSH_CALLS_DISCONNECT_GRACE_SECONDS` | how long a party device may be gone before its call fails, 5 to 120 | `30` |
/// | `PUSH_CALLS_HISTORY_DAYS` | how many days call history is kept, 1 to 3650; applies whether calls are enabled or not | `90` |
/// | `PUSH_AGENT_REFUSAL_MIN_S`, `PUSH_AGENT_REFUSAL_MAX_S` | a request an agent's policy refused stays pending a random time between these, in seconds (0 to 604800, min ≤ max), then ends like a decline | `600`, `21600` |
/// | `PUSH_AGENT_SWEEP_SECONDS` | how often due refusals are released, 1 to 300 | `30` |
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub database: DatabaseTarget,
    pub redis: RedisTarget,
    /// The identity, when a datastore uses Entra ID.
    pub azure: Option<AzureConfig>,
    pub run_migrations: bool,
    /// The Messaging gRPC API address.
    pub grpc_addr: SocketAddr,
    pub accounts_grpc_addr: SocketAddr,
    pub directory_grpc_addr: SocketAddr,
    pub health_addr: SocketAddr,
    pub store: StoreOptions,
    pub accounts_token: Option<String>,
    pub directory_token: Option<String>,
    pub messaging_token: Option<String>,
    pub calls: CallsConfig,
    pub agents: AgentsConfig,
}

/// Agent connections (spec 2026-11-06 agent connections §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentsConfig {
    /// The shortest time a refused request stays pending (`PUSH_AGENT_REFUSAL_MIN_S`).
    pub refusal_min: Duration,
    /// The longest (`PUSH_AGENT_REFUSAL_MAX_S`).
    pub refusal_max: Duration,
    /// How often due refusals are released (`PUSH_AGENT_SWEEP_SECONDS`).
    pub sweep: Duration,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            refusal_min: Duration::from_secs(600),
            refusal_max: Duration::from_secs(21600),
            sweep: Duration::from_secs(30),
        }
    }
}

impl AgentsConfig {
    fn from_lookup(lookup: &impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let refusal_min = seconds_in(lookup, "PUSH_AGENT_REFUSAL_MIN_S", 600, 0..=604_800)?;
        let refusal_max = seconds_in(lookup, "PUSH_AGENT_REFUSAL_MAX_S", 21600, 0..=604_800)?;
        if refusal_max < refusal_min {
            return Err(ConfigError::Invalid {
                name: "PUSH_AGENT_REFUSAL_MAX_S",
                value: refusal_max.as_secs().to_string(),
                reason: "must not be below PUSH_AGENT_REFUSAL_MIN_S".to_owned(),
            });
        }
        Ok(Self {
            refusal_min,
            refusal_max,
            sweep: seconds_in(lookup, "PUSH_AGENT_SWEEP_SECONDS", 30, 1..=300)?,
        })
    }
}

/// The call engine's settings (spec §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallsConfig {
    /// Whether message-svc offers calls (`PUSH_CALLS_ENABLED`).
    pub enabled: bool,
    /// How long a call rings (`PUSH_CALLS_RING_SECONDS`).
    pub ring: Duration,
    /// How long a party device may be gone before its call fails
    /// (`PUSH_CALLS_DISCONNECT_GRACE_SECONDS`).
    pub grace: Duration,
    /// How many days call history is kept (`PUSH_CALLS_HISTORY_DAYS`).
    pub history_days: u32,
}

impl Default for CallsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ring: Duration::from_secs(45),
            grace: Duration::from_secs(30),
            history_days: 90,
        }
    }
}

impl CallsConfig {
    fn from_lookup(lookup: &impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let enabled = match lookup("PUSH_CALLS_ENABLED").as_deref().map(str::trim) {
            None | Some("" | "0") => false,
            Some("1") => true,
            Some(other) => {
                return Err(ConfigError::Invalid {
                    name: "PUSH_CALLS_ENABLED",
                    value: other.to_owned(),
                    reason: "expected 1 or 0".to_owned(),
                });
            }
        };
        Ok(Self {
            enabled,
            ring: seconds_in(lookup, "PUSH_CALLS_RING_SECONDS", 45, 10..=120)?,
            grace: seconds_in(lookup, "PUSH_CALLS_DISCONNECT_GRACE_SECONDS", 30, 5..=120)?,
            history_days: history_days(lookup)?,
        })
    }
}

/// `PUSH_CALLS_HISTORY_DAYS`: 1 to 3650, 90 when unset or empty.
fn history_days(lookup: &impl Fn(&str) -> Option<String>) -> Result<u32, ConfigError> {
    const NAME: &str = "PUSH_CALLS_HISTORY_DAYS";
    let days: u32 = parsed(lookup, NAME, 90)?;
    if (1..=3650).contains(&days) {
        Ok(days)
    } else {
        Err(ConfigError::Invalid {
            name: NAME,
            value: days.to_string(),
            reason: "expected an integer from 1 to 3650".to_owned(),
        })
    }
}

/// A whole number of seconds within `range`, or `default` when unset or empty.
fn seconds_in(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: u64,
    range: std::ops::RangeInclusive<u64>,
) -> Result<Duration, ConfigError> {
    let seconds: u64 = parsed(lookup, name, default)?;
    if range.contains(&seconds) {
        Ok(Duration::from_secs(seconds))
    } else {
        Err(ConfigError::Invalid {
            name,
            value: seconds.to_string(),
            reason: format!(
                "expected an integer from {} to {}",
                range.start(),
                range.end()
            ),
        })
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("database", &self.database)
            .field("redis", &self.redis)
            .field("azure", &self.azure)
            .field("run_migrations", &self.run_migrations)
            .field("grpc_addr", &self.grpc_addr)
            .field("accounts_grpc_addr", &self.accounts_grpc_addr)
            .field("directory_grpc_addr", &self.directory_grpc_addr)
            .field("health_addr", &self.health_addr)
            .field("store", &self.store)
            .field(
                "accounts_token",
                &self.accounts_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "directory_token",
                &self.directory_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "messaging_token",
                &self.messaging_token.as_ref().map(|_| "<redacted>"),
            )
            .field("calls", &self.calls)
            .field("agents", &self.agents)
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
        let database = DatabaseTarget::from_lookup(
            lookup_dyn,
            mode,
            &DatabaseVars::own("push_messaging"),
            None,
        )
        .map_err(problem)?;
        let redis = RedisTarget::from_lookup(lookup_dyn).map_err(problem)?;
        let azure = azure_for(lookup_dyn, &[database.mode(), redis.mode()]).map_err(problem)?;
        let run_migrations = flag(&lookup, "PUSH_RUN_MIGRATIONS", true)?;
        let accounts_token = token(&lookup, "PUSH_ACCOUNTS_TOKEN")?;
        let directory_token = token(&lookup, "PUSH_DIRECTORY_TOKEN")?;
        if accounts_token.is_some() && accounts_token == directory_token {
            return Err(redacted(
                "PUSH_DIRECTORY_TOKEN",
                "must differ from PUSH_ACCOUNTS_TOKEN: each internal API has a token of its own"
                    .to_owned(),
            ));
        }
        let messaging_token = token(&lookup, "PUSH_MESSAGING_TOKEN")?;
        if messaging_token.is_some()
            && (messaging_token == accounts_token || messaging_token == directory_token)
        {
            return Err(redacted(
                "PUSH_MESSAGING_TOKEN",
                "must differ from PUSH_ACCOUNTS_TOKEN and PUSH_DIRECTORY_TOKEN: each internal \
                 API has a token of its own"
                    .to_owned(),
            ));
        }
        Ok(Self {
            database,
            redis,
            azure,
            run_migrations,
            grpc_addr: parsed(&lookup, "PUSH_GRPC_ADDR", ([0, 0, 0, 0], 50051).into())?,
            accounts_grpc_addr: parsed(
                &lookup,
                "PUSH_ACCOUNTS_GRPC_ADDR",
                ([0, 0, 0, 0], 50052).into(),
            )?,
            directory_grpc_addr: parsed(
                &lookup,
                "PUSH_DIRECTORY_GRPC_ADDR",
                ([0, 0, 0, 0], 50053).into(),
            )?,
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            store,
            accounts_token,
            directory_token,
            messaging_token,
            calls: CallsConfig::from_lookup(&lookup)?,
            agents: AgentsConfig::from_lookup(&lookup)?,
        })
    }
}

/// An internal token: unset or empty is `None`; otherwise it must pass [`check_token`].
fn token(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<Option<String>, ConfigError> {
    let token = push_credentials::application_secret(lookup, name).map_err(problem)?;
    if let Some(token) = &token {
        check_token(token).map_err(|reason| redacted(name, reason))?;
    }
    Ok(token)
}

/// A shared token fit for a header: 32 to 256 visible ASCII characters.
fn check_token(token: &str) -> Result<(), String> {
    if (32..=256).contains(&token.len()) && token.bytes().all(|byte| byte.is_ascii_graphic()) {
        Ok(())
    } else {
        Err("use 32 to 256 visible ASCII characters".to_owned())
    }
}

/// What `push-message-svc migrate` reads: the database variables above, the identity,
/// and `PUSH_DATABASE_RUNTIME_ROLE`, a role granted the use of the `messaging` schema
/// when the migrating identity is not the one the service runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateConfig {
    pub database: DatabaseTarget,
    pub azure: Option<AzureConfig>,
    pub runtime_role: Option<String>,
}

impl MigrateConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let lookup_dyn: &dyn Fn(&str) -> Option<String> = &lookup;
        let mode = postgres_auth(lookup_dyn).map_err(problem)?;
        let database = DatabaseTarget::from_lookup(
            lookup_dyn,
            mode,
            &DatabaseVars::own("push_messaging"),
            None,
        )
        .map_err(problem)?;
        let azure = azure_for(lookup_dyn, &[database.mode()]).map_err(problem)?;
        let runtime_role = runtime_role(&lookup)?;
        Ok(Self {
            database,
            azure,
            runtime_role,
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

/// An invalid URL variable; the value is never shown because it carries a password.
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

    #[test]
    fn mounted_internal_tokens_match_direct_configuration() {
        let directory = std::path::PathBuf::from("target")
            .join(format!("message-mounted-keys-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("token");
        for name in [
            "PUSH_ACCOUNTS_TOKEN",
            "PUSH_DIRECTORY_TOKEN",
            "PUSH_MESSAGING_TOKEN",
        ] {
            let value = "internal-token-of-the-configuration-tests";
            std::fs::write(&path, format!("{value}\n")).unwrap();
            let mut direct = REQUIRED.to_vec();
            direct.push((name, value));
            let expected = config(&direct).unwrap();
            direct.pop();
            let file_name = format!("{name}_FILE");
            direct.push((&file_name, path.to_str().unwrap()));
            assert_eq!(config(&direct).unwrap(), expected, "{name}");
            direct.push((name, value));
            let error = config(&direct).unwrap_err();
            assert!(!format!("{error:?} {error}").contains(value));
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }

    fn config(variables: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let variables: HashMap<String, String> = variables
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        Config::from_lookup(|name| variables.get(name).cloned())
    }

    const REQUIRED: [(&str, &str); 2] = [
        (
            "PUSH_DATABASE_URL",
            "postgres://push:dbsecret@db/push_messaging",
        ),
        ("PUSH_REDIS_URL", "redis://:secret@redis:6379"),
    ];

    #[test]
    fn defaults_apply_when_only_the_urls_are_set() {
        let config = config(&REQUIRED).unwrap();
        assert_eq!(
            config.database,
            DatabaseTarget::Url(REQUIRED[0].1.to_owned())
        );
        assert_eq!(
            config.redis,
            RedisTarget::Password {
                url: REQUIRED[1].1.to_owned()
            }
        );
        assert_eq!(config.grpc_addr, "0.0.0.0:50051".parse().unwrap());
        assert_eq!(config.accounts_grpc_addr, "0.0.0.0:50052".parse().unwrap());
        assert_eq!(config.directory_grpc_addr, "0.0.0.0:50053".parse().unwrap());
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.store, StoreOptions::default());
        assert_eq!(config.accounts_token, None);
        assert_eq!(config.directory_token, None);
        assert_eq!(config.messaging_token, None);
    }

    #[test]
    fn entra_mode_reads_the_parts_and_the_identity() {
        let variables = [
            ("PUSH_POSTGRES_AUTH", "azure-entra"),
            ("PUSH_DATABASE_HOST", "pg.example.net"),
            ("PUSH_DATABASE_USER", "push-message-svc-id"),
            ("PUSH_REDIS_AUTH", "azure-entra"),
            ("PUSH_REDIS_URL", "rediss://cache.example.net:10000"),
            ("AZURE_CLIENT_ID", "client"),
            ("AZURE_TENANT_ID", "tenant"),
            (
                "AZURE_FEDERATED_TOKEN_FILE",
                "/var/run/secrets/azure/tokens/azure-identity-token",
            ),
            ("PUSH_RUN_MIGRATIONS", "false"),
        ];
        let entra = config(&variables).unwrap();
        assert!(
            matches!(entra.database, DatabaseTarget::Entra(ref database) if database.name == "push_messaging")
        );
        assert!(matches!(entra.redis, RedisTarget::Entra { .. }));
        assert!(entra.azure.is_some());
        assert!(!entra.run_migrations);
        let defaults = config(&REQUIRED).unwrap();
        assert!(defaults.run_migrations && defaults.azure.is_none());
    }

    #[test]
    fn calls_are_off_by_default_with_the_spec_durations() {
        let defaults = config(&REQUIRED).unwrap();
        assert_eq!(
            defaults.calls,
            CallsConfig {
                enabled: false,
                ring: Duration::from_secs(45),
                grace: Duration::from_secs(30),
                history_days: 90,
            }
        );
        let mut variables = REQUIRED.to_vec();
        variables.extend([
            ("PUSH_CALLS_ENABLED", "1"),
            ("PUSH_CALLS_RING_SECONDS", "10"),
            ("PUSH_CALLS_DISCONNECT_GRACE_SECONDS", "120"),
            ("PUSH_CALLS_HISTORY_DAYS", "3650"),
        ]);
        let calls = config(&variables).unwrap().calls;
        assert!(calls.enabled);
        assert_eq!(calls.ring, Duration::from_secs(10));
        assert_eq!(calls.grace, Duration::from_secs(120));
        assert_eq!(calls.history_days, 3650);
        for off in ["0", ""] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_CALLS_ENABLED", off));
            assert!(!config(&variables).unwrap().calls.enabled, "{off:?}");
        }
    }

    #[test]
    fn calls_settings_out_of_range_name_their_variable() {
        let cases = [
            ("PUSH_CALLS_ENABLED", "true"),
            ("PUSH_CALLS_ENABLED", "yes"),
            ("PUSH_CALLS_RING_SECONDS", "9"),
            ("PUSH_CALLS_RING_SECONDS", "121"),
            ("PUSH_CALLS_RING_SECONDS", "forty"),
            ("PUSH_CALLS_DISCONNECT_GRACE_SECONDS", "4"),
            ("PUSH_CALLS_DISCONNECT_GRACE_SECONDS", "121"),
            ("PUSH_CALLS_DISCONNECT_GRACE_SECONDS", "-1"),
            ("PUSH_CALLS_HISTORY_DAYS", "0"),
            ("PUSH_CALLS_HISTORY_DAYS", "3651"),
            ("PUSH_CALLS_HISTORY_DAYS", "ninety"),
        ];
        for (name, value) in cases {
            let mut variables = REQUIRED.to_vec();
            variables.push((name, value));
            let error = config(&variables).unwrap_err();
            assert!(
                matches!(&error, ConfigError::Invalid { name: named, .. } if *named == name),
                "{name}={value}: {error}"
            );
            assert!(error.to_string().contains(name), "{error}");
        }
    }

    #[test]
    fn run_migrations_is_a_boolean() {
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_RUN_MIGRATIONS", "maybe"));
        assert!(matches!(
            config(&variables),
            Err(ConfigError::Invalid {
                name: "PUSH_RUN_MIGRATIONS",
                ..
            })
        ));
    }

    #[test]
    fn the_internal_tokens_are_checked_and_never_shown() {
        let (accounts, directory) = ("t0ken-".repeat(6), "d1rectory-".repeat(4));
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_ACCOUNTS_TOKEN", &accounts));
        variables.push(("PUSH_DIRECTORY_TOKEN", &directory));
        let configured = config(&variables).unwrap();
        assert_eq!(
            configured.accounts_token.as_deref(),
            Some(accounts.as_str())
        );
        assert_eq!(
            configured.directory_token.as_deref(),
            Some(directory.as_str())
        );
        let shown = format!("{configured:?}");
        assert!(!shown.contains(&accounts) && !shown.contains(&directory));
        for name in ["PUSH_ACCOUNTS_TOKEN", "PUSH_DIRECTORY_TOKEN"] {
            for bad in ["too-short", "with a space but long enough to pass 32"] {
                let mut variables = REQUIRED.to_vec();
                variables.push((name, bad));
                let message = config(&variables).unwrap_err().to_string();
                assert!(message.contains(name), "{message}");
                assert!(!message.contains(bad), "{message}");
            }
        }
    }

    #[test]
    fn the_two_internal_tokens_must_differ() {
        let shared = "s4me-".repeat(8);
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_ACCOUNTS_TOKEN", &shared));
        variables.push(("PUSH_DIRECTORY_TOKEN", &shared));
        let message = config(&variables).unwrap_err().to_string();
        assert!(message.contains("PUSH_ACCOUNTS_TOKEN"), "{message}");
        assert!(message.contains("PUSH_DIRECTORY_TOKEN"), "{message}");
        assert!(!message.contains(&shared), "{message}");
    }

    #[test]
    fn the_gateways_messaging_token_is_checked_and_differs_from_the_others() {
        let (accounts, directory, messaging) = (
            "t0ken-".repeat(6),
            "d1rectory-".repeat(4),
            "m3ssaging-".repeat(4),
        );
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_ACCOUNTS_TOKEN", &accounts));
        variables.push(("PUSH_DIRECTORY_TOKEN", &directory));
        variables.push(("PUSH_MESSAGING_TOKEN", &messaging));
        let configured = config(&variables).unwrap();
        assert_eq!(
            configured.messaging_token.as_deref(),
            Some(messaging.as_str())
        );
        assert!(!format!("{configured:?}").contains(&messaging));
        for shared in [&accounts, &directory] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_ACCOUNTS_TOKEN", &accounts));
            variables.push(("PUSH_DIRECTORY_TOKEN", &directory));
            variables.push(("PUSH_MESSAGING_TOKEN", shared));
            let message = config(&variables).unwrap_err().to_string();
            assert!(message.contains("PUSH_MESSAGING_TOKEN"), "{message}");
            assert!(!message.contains(shared.as_str()), "{message}");
        }
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_MESSAGING_TOKEN", "too-short"));
        assert!(config(&variables).is_err());
    }

    #[test]
    fn the_urls_are_required() {
        assert_eq!(
            config(&REQUIRED[1..]),
            Err(ConfigError::Missing("PUSH_DATABASE_URL"))
        );
        assert_eq!(
            config(&[REQUIRED[0], ("PUSH_REDIS_URL", "")]),
            Err(ConfigError::Missing("PUSH_REDIS_URL"))
        );
    }

    #[test]
    fn addresses_and_pool_size_can_be_set() {
        let mut variables = REQUIRED.to_vec();
        variables.extend([
            ("PUSH_GRPC_ADDR", "127.0.0.1:6000"),
            ("PUSH_ACCOUNTS_GRPC_ADDR", "127.0.0.1:6002"),
            ("PUSH_DIRECTORY_GRPC_ADDR", "127.0.0.1:6003"),
            ("PUSH_HEALTH_ADDR", "127.0.0.1:6001"),
            ("PUSH_DB_MAX_CONNECTIONS", "3"),
        ]);
        let config = config(&variables).unwrap();
        assert_eq!(config.grpc_addr, "127.0.0.1:6000".parse().unwrap());
        assert_eq!(config.accounts_grpc_addr, "127.0.0.1:6002".parse().unwrap());
        assert_eq!(
            config.directory_grpc_addr,
            "127.0.0.1:6003".parse().unwrap()
        );
        assert_eq!(config.health_addr, "127.0.0.1:6001".parse().unwrap());
        assert_eq!(config.store.max_connections, 3);
    }

    #[test]
    fn unusable_values_fail_start_up_without_echoing_the_urls() {
        let bad = |name: &str, value: &str| {
            let mut variables = REQUIRED.to_vec();
            variables.retain(|(key, _)| *key != name);
            variables.push((name, value));
            config(&variables).unwrap_err().to_string()
        };
        let message = bad("PUSH_DATABASE_URL", "postgres://push:hunter2@db:notaport/x");
        assert!(message.contains("PUSH_DATABASE_URL"), "{message}");
        assert!(!message.contains("hunter2"), "{message}");
        let message = bad("PUSH_DATABASE_URL", "mysql://push:hunter2@db/x");
        assert!(message.contains("PUSH_DATABASE_URL"), "{message}");
        assert!(!message.contains("hunter2"), "{message}");
        let message = bad("PUSH_REDIS_URL", "http://:hunter2@redis:6379");
        assert!(message.contains("PUSH_REDIS_URL"), "{message}");
        assert!(!message.contains("hunter2"), "{message}");
        let message = bad("PUSH_DB_MAX_CONNECTIONS", "0");
        assert!(message.contains("PUSH_DB_MAX_CONNECTIONS"), "{message}");
    }

    #[test]
    fn debug_output_hides_the_urls() {
        let config = config(&REQUIRED).unwrap();
        let shown = format!("{config:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
    }

    #[test]
    fn malformed_additional_api_addresses_are_not_ignored() {
        for name in ["PUSH_ACCOUNTS_GRPC_ADDR", "PUSH_DIRECTORY_GRPC_ADDR"] {
            let mut variables = REQUIRED.to_vec();
            variables.push((name, "not-an-address"));
            assert!(matches!(
                config(&variables),
                Err(ConfigError::Invalid { name: bad, .. }) if bad == name
            ));
        }
    }

    #[test]
    fn a_malformed_address_is_refused() {
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_GRPC_ADDR", "not-an-address"));
        assert!(matches!(
            config(&variables),
            Err(ConfigError::Invalid {
                name: "PUSH_GRPC_ADDR",
                ..
            })
        ));
    }
}
