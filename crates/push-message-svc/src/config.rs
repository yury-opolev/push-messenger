//! Configuration from environment variables.

use std::net::SocketAddr;

use push_store::{StoreOptions, check_database_url};

/// How the service runs. Read from the environment by [`Config::from_env`]:
///
/// | Variable | Meaning | Default |
/// |---|---|---|
/// | `PUSH_DATABASE_URL` | Postgres URL of the `push_messaging` database, as its owner | required |
/// | `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
/// | `PUSH_GRPC_ADDR` | address of the gRPC API | `0.0.0.0:50051` |
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_DEV_SETUP` | `1` registers the DEV ONLY `DevSetup` service | off |
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub database_url: String,
    pub redis_url: String,
    pub grpc_addr: SocketAddr,
    pub health_addr: SocketAddr,
    pub store: StoreOptions,
    pub dev_setup: bool,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("database_url", &"<redacted>")
            .field("redis_url", &"<redacted>")
            .field("grpc_addr", &self.grpc_addr)
            .field("health_addr", &self.health_addr)
            .field("store", &self.store)
            .field("dev_setup", &self.dev_setup)
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
        let database_url = required("PUSH_DATABASE_URL")?;
        check_database_url(&database_url)
            .map_err(|reason| redacted("PUSH_DATABASE_URL", reason))?;
        let redis_url = required("PUSH_REDIS_URL")?;
        redis::Client::open(redis_url.as_str()).map_err(|_| {
            redacted(
                "PUSH_REDIS_URL",
                "not a valid Redis URL, expected redis://[:password@]host:6379".to_owned(),
            )
        })?;
        Ok(Self {
            database_url,
            redis_url,
            grpc_addr: parsed(&lookup, "PUSH_GRPC_ADDR", ([0, 0, 0, 0], 50051).into())?,
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            store,
            dev_setup: dev_flag(&lookup, "PUSH_DEV_SETUP")?,
        })
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

/// A DEV ONLY switch: on only for exactly `1`; off when unset, empty or `0`. Anything
/// else (`true`, `yes`) is refused, so a typo can never switch it on or leave it in doubt.
fn dev_flag(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<bool, ConfigError> {
    match lookup(name).as_deref() {
        None | Some("") | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(ConfigError::Invalid {
            name,
            value: other.to_owned(),
            reason: "use 1 to enable or 0 to disable".to_owned(),
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

    const REQUIRED: [(&str, &str); 2] = [
        ("PUSH_DATABASE_URL", "postgres://push@db/push_messaging"),
        ("PUSH_REDIS_URL", "redis://:secret@redis:6379"),
    ];

    #[test]
    fn defaults_apply_when_only_the_urls_are_set() {
        let config = config(&REQUIRED).unwrap();
        assert_eq!(config.database_url, "postgres://push@db/push_messaging");
        assert_eq!(config.redis_url, "redis://:secret@redis:6379");
        assert_eq!(config.grpc_addr, "0.0.0.0:50051".parse().unwrap());
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.store, StoreOptions::default());
        assert!(!config.dev_setup);
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
            ("PUSH_HEALTH_ADDR", "127.0.0.1:6001"),
            ("PUSH_DB_MAX_CONNECTIONS", "3"),
        ]);
        let config = config(&variables).unwrap();
        assert_eq!(config.grpc_addr, "127.0.0.1:6000".parse().unwrap());
        assert_eq!(config.health_addr, "127.0.0.1:6001".parse().unwrap());
        assert_eq!(config.store.max_connections, 3);
    }

    #[test]
    fn dev_setup_is_on_only_for_exactly_1() {
        for (value, expected) in [("1", true), ("0", false), ("", false)] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_DEV_SETUP", value));
            assert_eq!(config(&variables).unwrap().dev_setup, expected, "{value:?}");
        }
        for value in ["true", "yes", " 1"] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_DEV_SETUP", value));
            assert!(
                matches!(
                    config(&variables),
                    Err(ConfigError::Invalid {
                        name: "PUSH_DEV_SETUP",
                        ..
                    })
                ),
                "{value:?}"
            );
        }
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
