//! Configuration from environment variables.

use std::net::SocketAddr;

use push_store::{StoreOptions, check_database_url};

/// How the service runs. Read from the environment by [`Config::from_env`]:
///
/// | Variable | Meaning | Default |
/// |---|---|---|
/// | `PUSH_DATABASE_URL` | Postgres URL of the `push_messaging` database, as its owner | required |
/// | `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
/// | `PUSH_GRPC_ADDR` | address of the `Messaging` gRPC API | `0.0.0.0:50051` |
/// | `PUSH_ACCOUNTS_GRPC_ADDR` | address of the `Accounts` gRPC API | `0.0.0.0:50052` |
/// | `PUSH_DIRECTORY_GRPC_ADDR` | address of the `Directory` gRPC API | `0.0.0.0:50053` |
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_DB_MAX_CONNECTIONS` | pool size, at least 1 | `10` |
/// | `PUSH_ACCOUNTS_TOKEN` | the worker's token for the `Accounts` API, 32 to 256 visible ASCII characters; without it `Accounts` refuses every call | none |
/// | `PUSH_DIRECTORY_TOKEN` | auth-svc's token for the `Directory` API, 32 to 256 visible ASCII characters and different from `PUSH_ACCOUNTS_TOKEN`; without it `Directory` refuses every call | none |
/// | `PUSH_MESSAGING_TOKEN` | the gateway's token for the `Messaging` API, 32 to 256 visible ASCII characters and different from the other two; without it `Messaging` refuses every call | none |
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub database_url: String,
    pub redis_url: String,
    /// The Messaging gRPC API address.
    pub grpc_addr: SocketAddr,
    pub accounts_grpc_addr: SocketAddr,
    pub directory_grpc_addr: SocketAddr,
    pub health_addr: SocketAddr,
    pub store: StoreOptions,
    pub accounts_token: Option<String>,
    pub directory_token: Option<String>,
    pub messaging_token: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("database_url", &"<redacted>")
            .field("redis_url", &"<redacted>")
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
            database_url,
            redis_url,
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
        })
    }
}

/// An internal token: unset or empty is `None`; otherwise it must pass [`check_token`].
fn token(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<Option<String>, ConfigError> {
    let token = lookup(name).filter(|value| !value.is_empty());
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
            config.database_url,
            "postgres://push:dbsecret@db/push_messaging"
        );
        assert_eq!(config.redis_url, "redis://:secret@redis:6379");
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
