//! Configuration from environment variables.

use std::net::SocketAddr;

use axum::http::HeaderName;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use push_store::{StoreOptions, check_database_url};

/// How the service runs. Read from the environment by [`Config::from_env`]:
///
/// | Variable | Meaning | Default |
/// |---|---|---|
/// | `PUSH_DATABASE_URL` | Postgres URL of the `push_auth` database, as its owner | required |
/// | `PUSH_REDIS_URL` | Redis URL for rate limits, `redis://:password@host:6379` | required |
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
/// | `PUSH_DEV_MAILBOX` | `1` serves the DEV ONLY `GET /dev/mailbox` (local public URL only) | off |
///
/// `http://` public URLs and `ws://` audiences are accepted only for local hosts
/// (`localhost`, `127.0.0.1`, `*.local`, `*.localhost`, `*.test`), and so is the DEV ONLY
/// mailbox, like the chart's dev-mode guard.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub database_url: String,
    pub redis_url: String,
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
    pub dev_mailbox: bool,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("database_url", &"<redacted>")
            .field("redis_url", &"<redacted>")
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
            .field("dev_mailbox", &self.dev_mailbox)
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
        let dev_mailbox = dev_flag(&lookup, "PUSH_DEV_MAILBOX")?;
        if dev_mailbox && !is_local_url(&public_url) {
            return Err(ConfigError::Invalid {
                name: "PUSH_DEV_MAILBOX",
                value: "1".to_owned(),
                reason: "the DEV ONLY mailbox shows every account's email links; it is only \
                         allowed with a local PUSH_AUTH_PUBLIC_URL host (localhost, 127.0.0.1, \
                         *.local, *.localhost, *.test)"
                    .to_owned(),
            });
        }
        Ok(Self {
            database_url,
            redis_url,
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
            dev_mailbox,
        })
    }
}

/// Whether the host of `url`, a checked public URL, is local.
fn is_local_url(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    is_local_host(host_of(rest))
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
        assert!(!config.dev_mailbox);
    }

    #[test]
    fn every_required_variable_is_required() {
        for (index, (name, _)) in REQUIRED.iter().enumerate() {
            let mut variables = REQUIRED.to_vec();
            variables.remove(index);
            assert_eq!(config(&variables), Err(ConfigError::Missing(name)));
        }
    }

    /// The required variables with a local public URL, and `name` set to `value`.
    fn local_with(name: &'static str, value: &'static str) -> Vec<(&'static str, &'static str)> {
        let mut variables = with("PUSH_AUTH_PUBLIC_URL", "https://push.test");
        variables.retain(|(key, _)| *key != name);
        variables.push((name, value));
        variables
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
            ("PUSH_DEV_MAILBOX", "1"),
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
        assert!(config.dev_mailbox);
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
    fn dev_mailbox_is_on_only_for_exactly_1() {
        for (value, expected) in [("1", true), ("0", false), ("", false)] {
            assert_eq!(
                config(&local_with("PUSH_DEV_MAILBOX", value))
                    .unwrap()
                    .dev_mailbox,
                expected
            );
        }
        for value in ["true", "yes", " 1"] {
            assert!(
                config(&local_with("PUSH_DEV_MAILBOX", value)).is_err(),
                "{value:?}"
            );
        }
    }

    #[test]
    fn the_dev_mailbox_needs_a_local_public_url() {
        for url in [
            "http://localhost:8080",
            "http://127.0.0.1",
            "https://push.local",
            "http://push.localhost/",
            "https://push.test",
        ] {
            let mut variables = local_with("PUSH_DEV_MAILBOX", "1");
            variables.retain(|(key, _)| *key != "PUSH_AUTH_PUBLIC_URL");
            variables.push(("PUSH_AUTH_PUBLIC_URL", url));
            assert!(config(&variables).unwrap().dev_mailbox, "{url}");
        }
        for url in [
            "https://api.example.com",
            "https://localhost.example.com",
            "https://push.test.example.com",
        ] {
            let mut variables = with("PUSH_DEV_MAILBOX", "1");
            variables.retain(|(key, _)| *key != "PUSH_AUTH_PUBLIC_URL");
            variables.push(("PUSH_AUTH_PUBLIC_URL", url));
            assert!(
                matches!(
                    config(&variables),
                    Err(ConfigError::Invalid {
                        name: "PUSH_DEV_MAILBOX",
                        ..
                    })
                ),
                "{url}"
            );
        }
        for url in [
            "https://evil.com\\.test",
            "https://user@evil.com",
            "http://evil.com@push.test",
        ] {
            let mut variables = with("PUSH_DEV_MAILBOX", "1");
            variables.retain(|(key, _)| *key != "PUSH_AUTH_PUBLIC_URL");
            variables.push(("PUSH_AUTH_PUBLIC_URL", url));
            assert!(config(&variables).is_err(), "{url}");
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
    fn zero_hash_concurrency_is_refused() {
        assert!(config(&with("PUSH_AUTH_HASH_CONCURRENCY", "0")).is_err());
    }
}
