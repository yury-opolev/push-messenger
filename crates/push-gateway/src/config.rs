//! Configuration from environment variables.

use std::net::SocketAddr;
use std::time::Duration;

/// How the gateway runs. Read from the environment by [`Config::from_env`]:
///
/// | Variable | Meaning | Default |
/// |---|---|---|
/// | `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc | required |
/// | `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
/// | `PUSH_LISTEN_ADDR` | address of the WebSocket endpoint `/v1/socket` | `0.0.0.0:8080` |
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready` | `30000` |
/// | `PUSH_DEV_AUTH` | `1` accepts DEV ONLY identities without a token | off |
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub message_svc_url: String,
    pub redis_url: String,
    pub listen_addr: SocketAddr,
    pub health_addr: SocketAddr,
    pub heartbeat: Duration,
    pub dev_auth: bool,
}

// The Redis URL carries a password, so it never appears in debug output.
impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("message_svc_url", &self.message_svc_url)
            .field("redis_url", &"<redacted>")
            .field("listen_addr", &self.listen_addr)
            .field("health_addr", &self.health_addr)
            .field("heartbeat", &self.heartbeat)
            .field("dev_auth", &self.dev_auth)
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
        let heartbeat_ms: u64 = parsed(&lookup, "PUSH_HEARTBEAT_MS", 30_000)?;
        if !(1_000..=600_000).contains(&heartbeat_ms) {
            return Err(ConfigError::Invalid {
                name: "PUSH_HEARTBEAT_MS",
                value: heartbeat_ms.to_string(),
                reason: "use 1000 to 600000 milliseconds".to_owned(),
            });
        }
        Ok(Self {
            message_svc_url: required("PUSH_MESSAGE_SVC_URL")?,
            redis_url: required("PUSH_REDIS_URL")?,
            listen_addr: parsed(&lookup, "PUSH_LISTEN_ADDR", ([0, 0, 0, 0], 8080).into())?,
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            heartbeat: Duration::from_millis(heartbeat_ms),
            dev_auth: dev_flag(&lookup, "PUSH_DEV_AUTH")?,
        })
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
        ("PUSH_MESSAGE_SVC_URL", "http://message-svc:50051"),
        ("PUSH_REDIS_URL", "redis://:secret@redis:6379"),
    ];

    #[test]
    fn defaults_apply_when_only_the_urls_are_set() {
        let config = config(&REQUIRED).unwrap();
        assert_eq!(config.message_svc_url, "http://message-svc:50051");
        assert_eq!(config.redis_url, "redis://:secret@redis:6379");
        assert_eq!(config.listen_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.heartbeat, Duration::from_secs(30));
        assert!(!config.dev_auth);
    }

    #[test]
    fn the_urls_are_required() {
        assert_eq!(
            config(&REQUIRED[1..]),
            Err(ConfigError::Missing("PUSH_MESSAGE_SVC_URL"))
        );
        assert_eq!(
            config(&REQUIRED[..1]),
            Err(ConfigError::Missing("PUSH_REDIS_URL"))
        );
    }

    #[test]
    fn the_heartbeat_must_be_between_one_second_and_ten_minutes() {
        for (value, ok) in [
            ("1000", true),
            ("600000", true),
            ("999", false),
            ("600001", false),
        ] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_HEARTBEAT_MS", value));
            assert_eq!(config(&variables).is_ok(), ok, "{value}");
        }
    }

    #[test]
    fn dev_auth_is_on_only_for_exactly_1() {
        for (value, expected) in [("1", true), ("0", false), ("", false)] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_DEV_AUTH", value));
            assert_eq!(config(&variables).unwrap().dev_auth, expected, "{value:?}");
        }
        for value in ["true", "yes", "on", " 1"] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_DEV_AUTH", value));
            assert!(
                matches!(
                    config(&variables),
                    Err(ConfigError::Invalid {
                        name: "PUSH_DEV_AUTH",
                        ..
                    })
                ),
                "{value:?}"
            );
        }
    }
}
