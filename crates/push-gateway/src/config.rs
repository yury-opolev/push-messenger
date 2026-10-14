//! Configuration from environment variables.

use std::net::SocketAddr;
use std::time::Duration;

use axum::http::HeaderName;

/// How the gateway runs. Read from the environment by [`Config::from_env`]:
///
/// | Variable | Meaning | Default |
/// |---|---|---|
/// | `PUSH_MESSAGE_SVC_URL` | gRPC URL of message-svc | required |
/// | `PUSH_MESSAGING_TOKEN` | message-svc's token for its `Messaging` API, the same value as message-svc's: 32 to 256 visible ASCII characters | required |
/// | `PUSH_REDIS_URL` | Redis URL, `redis://:password@host:6379` | required |
/// | `PUSH_AUTH_URL` | base URL of auth-svc, whose `/v1/auth/jwks` holds the token keys | required |
/// | `PUSH_TOKEN_AUDIENCE` | the gateway's public URL (`wss://…/v1/socket`): the `aud` of the access tokens it accepts | required |
/// | `PUSH_LISTEN_ADDR` | address of the WebSocket endpoint `/v1/socket` | `0.0.0.0:8080` |
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_HEARTBEAT_MS` | client ping interval announced in `Ready` | `30000` |
/// | `PUSH_GROUPS` | `1` serves group requests; `0` refuses only the six group-management requests (create, add and remove members, leave, rename, set role) with `NOT_AVAILABLE`; groups that already exist still reach clients | `0` |
/// | `PUSH_CLIENT_IP_HEADER` | header with the client's IP, set by the ingress (`x-real-ip`); its last line, then last entry is used | the peer address |
///
/// `PUSH_DEV_AUTH=1` (the DEV ONLY identities without a token, removed in Plan 8) is
/// refused, so a deployment that still expects it fails at start-up.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub message_svc_url: String,
    /// The gateway's token for message-svc's `Messaging` API. Never shown.
    pub messaging_token: String,
    pub redis_url: String,
    pub auth_url: String,
    pub token_audience: String,
    pub listen_addr: SocketAddr,
    pub health_addr: SocketAddr,
    pub heartbeat: Duration,
    /// Whether group requests are served.
    pub groups: bool,
    /// Where the ingress puts the client's IP, for the per-IP rate limits; without it the
    /// peer address counts. Only trust a header the proxy in front overwrites.
    pub client_ip_header: Option<HeaderName>,
}

// The Redis URL carries a password and the token is a secret, so neither appears in debug
// output.
impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("message_svc_url", &self.message_svc_url)
            .field("messaging_token", &"<redacted>")
            .field("redis_url", &"<redacted>")
            .field("auth_url", &self.auth_url)
            .field("token_audience", &self.token_audience)
            .field("listen_addr", &self.listen_addr)
            .field("health_addr", &self.health_addr)
            .field("heartbeat", &self.heartbeat)
            .field("groups", &self.groups)
            .field("client_ip_header", &self.client_ip_header)
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
        match lookup("PUSH_DEV_AUTH").as_deref() {
            None | Some("") | Some("0") => {}
            Some(other) => {
                return Err(ConfigError::Invalid {
                    name: "PUSH_DEV_AUTH",
                    value: other.to_owned(),
                    reason: "DEV ONLY identities were removed: clients identify with access \
                             tokens"
                        .to_owned(),
                });
            }
        }
        let client_ip_header =
            match lookup("PUSH_CLIENT_IP_HEADER").filter(|v| !v.is_empty()) {
                None => None,
                Some(value) => Some(HeaderName::try_from(value.as_str()).map_err(|_| {
                    ConfigError::Invalid {
                        name: "PUSH_CLIENT_IP_HEADER",
                        value: value.clone(),
                        reason: "not a header name".to_owned(),
                    }
                })?),
            };
        let auth_url = required("PUSH_AUTH_URL")?;
        if !(auth_url.starts_with("http://") || auth_url.starts_with("https://")) {
            return Err(ConfigError::Invalid {
                name: "PUSH_AUTH_URL",
                value: auth_url,
                reason: "use an http:// or https:// URL".to_owned(),
            });
        }
        let messaging_token = required("PUSH_MESSAGING_TOKEN")?;
        if !((32..=256).contains(&messaging_token.len())
            && messaging_token.bytes().all(|byte| byte.is_ascii_graphic()))
        {
            return Err(ConfigError::Invalid {
                name: "PUSH_MESSAGING_TOKEN",
                value: "<redacted>".to_owned(),
                reason: "use 32 to 256 visible ASCII characters".to_owned(),
            });
        }
        Ok(Self {
            message_svc_url: required("PUSH_MESSAGE_SVC_URL")?,
            messaging_token,
            redis_url: required("PUSH_REDIS_URL")?,
            auth_url,
            token_audience: required("PUSH_TOKEN_AUDIENCE")?,
            listen_addr: parsed(&lookup, "PUSH_LISTEN_ADDR", ([0, 0, 0, 0], 8080).into())?,
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            heartbeat: Duration::from_millis(heartbeat_ms),
            groups: flag(&lookup, "PUSH_GROUPS")?,
            client_ip_header,
        })
    }

    /// auth-svc's JSON Web Key Set: the keys that sign access tokens.
    pub fn jwks_url(&self) -> String {
        format!("{}/v1/auth/jwks", self.auth_url.trim_end_matches('/'))
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

/// A switch: on only for exactly `1`; off when unset, empty or `0`. Anything else
/// (`true`, `yes`) is refused, so a typo can never leave it in doubt.
fn flag(lookup: &impl Fn(&str) -> Option<String>, name: &'static str) -> Result<bool, ConfigError> {
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

    const REQUIRED: [(&str, &str); 5] = [
        ("PUSH_MESSAGE_SVC_URL", "http://message-svc:50051"),
        (
            "PUSH_MESSAGING_TOKEN",
            "messaging-token-of-the-config-tests",
        ),
        ("PUSH_REDIS_URL", "redis://:secret@redis:6379"),
        ("PUSH_AUTH_URL", "http://auth:8080/"),
        ("PUSH_TOKEN_AUDIENCE", "wss://push.example.com/v1/socket"),
    ];

    #[test]
    fn defaults_apply_when_only_the_urls_are_set() {
        let config = config(&REQUIRED).unwrap();
        assert_eq!(config.message_svc_url, "http://message-svc:50051");
        assert_eq!(config.redis_url, "redis://:secret@redis:6379");
        assert_eq!(config.jwks_url(), "http://auth:8080/v1/auth/jwks");
        assert_eq!(config.token_audience, "wss://push.example.com/v1/socket");
        assert_eq!(config.listen_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.heartbeat, Duration::from_secs(30));
        assert!(!config.groups, "groups are off unless switched on");
        assert_eq!(config.client_ip_header, None);
    }

    #[test]
    fn the_client_ip_header_is_a_header_name() {
        let mut variables = REQUIRED.to_vec();
        variables.push(("PUSH_CLIENT_IP_HEADER", "X-Real-IP"));
        assert_eq!(
            config(&variables).unwrap().client_ip_header,
            Some(HeaderName::from_static("x-real-ip"))
        );
        variables.pop();
        variables.push(("PUSH_CLIENT_IP_HEADER", "not a header"));
        assert!(config(&variables).is_err());
    }

    #[test]
    fn groups_are_on_only_for_exactly_1() {
        for (value, expected) in [("1", true), ("0", false), ("", false)] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_GROUPS", value));
            assert_eq!(config(&variables).unwrap().groups, expected, "{value:?}");
        }
        for value in ["true", "yes", " 1"] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_GROUPS", value));
            assert!(config(&variables).is_err(), "{value:?}");
        }
    }

    #[test]
    fn debug_output_redacts_the_redis_url_and_the_token() {
        let output = format!("{:?}", config(&REQUIRED).unwrap());
        assert!(!output.contains("secret"), "{output}");
        assert!(!output.contains("messaging-token"), "{output}");
        assert!(output.contains("<redacted>"), "{output}");
    }

    #[test]
    fn the_messaging_token_must_be_fit_for_a_header_and_is_never_echoed() {
        for bad in ["too-short", "with a space but long enough to pass the 32"] {
            let mut variables = REQUIRED.to_vec();
            variables[1] = ("PUSH_MESSAGING_TOKEN", bad);
            let message = config(&variables).unwrap_err().to_string();
            assert!(message.contains("PUSH_MESSAGING_TOKEN"), "{message}");
            assert!(!message.contains(bad), "{message}");
        }
    }

    #[test]
    fn the_urls_and_the_audience_are_required() {
        for (index, (name, _)) in REQUIRED.iter().enumerate() {
            let mut variables = REQUIRED.to_vec();
            variables.remove(index);
            assert_eq!(
                config(&variables),
                Err(ConfigError::Missing(name)),
                "{name}"
            );
        }
        let mut variables = REQUIRED.to_vec();
        variables[3] = ("PUSH_AUTH_URL", "auth:8080");
        assert!(matches!(
            config(&variables),
            Err(ConfigError::Invalid {
                name: "PUSH_AUTH_URL",
                ..
            })
        ));
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
    fn dev_identities_are_gone_and_asking_for_them_is_refused() {
        for value in ["0", ""] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_DEV_AUTH", value));
            assert!(config(&variables).is_ok(), "{value:?}");
        }
        for value in ["1", "true", "yes"] {
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
