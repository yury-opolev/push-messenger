//! Configuration from environment variables.

use std::net::SocketAddr;
use std::time::Duration;

use push_store::{StoreOptions, check_database_url};

/// Which sender delivers email.
#[derive(Clone, PartialEq, Eq)]
pub enum Sender {
    /// Logs each email instead of sending it (local development).
    Log,
    /// Resend's API.
    Resend { api_url: String, api_key: String },
}

impl std::fmt::Debug for Sender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Log => formatter.write_str("Log"),
            Self::Resend { api_url, .. } => formatter
                .debug_struct("Resend")
                .field("api_url", api_url)
                .field("api_key", &"<redacted>")
                .finish(),
        }
    }
}

/// How the worker runs. Read from the environment by [`Config::from_env`]:
///
/// | Variable | Meaning | Default |
/// |---|---|---|
/// | `PUSH_DATABASE_URL` | Postgres URL of the `push_worker` database, as its owner | required |
/// | `PUSH_AUTH_DATABASE_URL` | Postgres URL of auth-svc's `push_auth` database, as the worker's role | required |
/// | `PUSH_MESSAGE_SVC_URL` | Accounts gRPC URL of message-svc, `http://host:50052` (plain `http://` only: the worker is built without TLS) | required |
/// | `PUSH_ACCOUNTS_TOKEN` | the token message-svc requires for its `Accounts` API, 32 to 256 visible ASCII characters | required |
/// | `PUSH_EMAIL_SENDER` | `log` (logs instead of sending) or `resend` | `log` |
/// | `PUSH_EMAIL_FROM` | the From of every email, `Push Messenger <no-reply@push.kubegames.dk>` | required for `resend` |
/// | `PUSH_EMAIL_PRODUCT_NAME` | the name emails sign with; no control characters, since it goes into the subject | `Push Messenger` |
/// | `PUSH_RESEND_API_KEY` | Resend API key | required for `resend` |
/// | `PUSH_RESEND_API_URL` | Resend's API (`http` only for local hosts, for tests) | `https://api.resend.com` |
/// | `PUSH_RESEND_WEBHOOK_SECRET` | the webhook's signing secret, `whsec_…`; without it `/v1/webhooks/resend` is not served | none |
/// | `PUSH_WORKER_POLL_MS` | how often an idle worker looks for work, in milliseconds | `1000` |
/// | `PUSH_UNVERIFIED_ACCOUNT_DAYS` | accounts unverified for this many days are deleted | `7` |
/// | `PUSH_WORKER_RETENTION_DAYS` | buried jobs, email records and delivery events are kept this many days | `30` |
/// | `PUSH_HTTP_ADDR` | address of the webhook endpoint (and the DEV ONLY mailbox) | `0.0.0.0:8080` |
/// | `PUSH_HEALTH_ADDR` | address of `/healthz` and `/readyz` | `0.0.0.0:8081` |
/// | `PUSH_DB_MAX_CONNECTIONS` | size of each of the two pools, at least 1 | `5` |
/// | `PUSH_DEV_MAILBOX` | `1` serves the DEV ONLY `GET /dev/mailbox` (log sender only) | off |
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub database_url: String,
    pub auth_database_url: String,
    pub message_svc_url: String,
    pub accounts_token: String,
    pub sender: Sender,
    pub from: String,
    pub product_name: String,
    pub webhook_secret: Option<String>,
    pub poll: Duration,
    pub unverified_after: Duration,
    pub retention: Duration,
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
            .field("auth_database_url", &"<redacted>")
            .field("message_svc_url", &self.message_svc_url)
            .field("accounts_token", &"<redacted>")
            .field("sender", &self.sender)
            .field("from", &self.from)
            .field("product_name", &self.product_name)
            .field(
                "webhook_secret",
                &self.webhook_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("poll", &self.poll)
            .field("unverified_after", &self.unverified_after)
            .field("retention", &self.retention)
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

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

impl Config {
    /// Reads the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads variables through `lookup`, which returns a variable's value if it is set.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let set = |name: &str| lookup(name).filter(|value| !value.is_empty());
        let required = |name: &'static str| set(name).ok_or(ConfigError::Missing(name));
        let store = StoreOptions {
            max_connections: parsed(&lookup, "PUSH_DB_MAX_CONNECTIONS", 5)?,
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
        let auth_database_url = required("PUSH_AUTH_DATABASE_URL")?;
        check_database_url(&auth_database_url)
            .map_err(|reason| redacted("PUSH_AUTH_DATABASE_URL", reason))?;
        let message_svc_url = required("PUSH_MESSAGE_SVC_URL")?;
        if !message_svc_url.starts_with("http://") {
            return Err(ConfigError::Invalid {
                name: "PUSH_MESSAGE_SVC_URL",
                value: message_svc_url,
                reason: "must start with http:// (the worker speaks plain gRPC to message-svc \
                         inside the cluster and is built without TLS)"
                    .to_owned(),
            });
        }
        let accounts_token = required("PUSH_ACCOUNTS_TOKEN")?;
        if !((32..=256).contains(&accounts_token.len())
            && accounts_token.bytes().all(|byte| byte.is_ascii_graphic()))
        {
            return Err(redacted(
                "PUSH_ACCOUNTS_TOKEN",
                "use 32 to 256 visible ASCII characters".to_owned(),
            ));
        }
        let sender = match set("PUSH_EMAIL_SENDER").as_deref() {
            None | Some("log") => Sender::Log,
            Some("resend") => {
                let api_url = set("PUSH_RESEND_API_URL")
                    .unwrap_or_else(|| push_email::RESEND_API_URL.to_owned());
                if !(api_url.starts_with("https://") || is_local_http(&api_url)) {
                    return Err(ConfigError::Invalid {
                        name: "PUSH_RESEND_API_URL",
                        value: api_url,
                        reason:
                            "must start with https:// (http:// only for 127.0.0.1 or localhost)"
                                .to_owned(),
                    });
                }
                Sender::Resend {
                    api_url,
                    api_key: required("PUSH_RESEND_API_KEY")?,
                }
            }
            Some(other) => {
                return Err(ConfigError::Invalid {
                    name: "PUSH_EMAIL_SENDER",
                    value: other.to_owned(),
                    reason: "use log or resend".to_owned(),
                });
            }
        };
        let from = match sender {
            Sender::Log => set("PUSH_EMAIL_FROM").unwrap_or_default(),
            Sender::Resend { .. } => required("PUSH_EMAIL_FROM")?,
        };
        if from.chars().any(char::is_control) {
            return Err(ConfigError::Invalid {
                name: "PUSH_EMAIL_FROM",
                value: from,
                reason: "must not contain control characters (it goes into an email header)"
                    .to_owned(),
            });
        }
        let product_name =
            set("PUSH_EMAIL_PRODUCT_NAME").unwrap_or_else(|| "Push Messenger".to_owned());
        if product_name.chars().any(char::is_control) {
            return Err(ConfigError::Invalid {
                name: "PUSH_EMAIL_PRODUCT_NAME",
                value: product_name,
                reason: "must not contain control characters (it goes into the email subject)"
                    .to_owned(),
            });
        }
        let webhook_secret = set("PUSH_RESEND_WEBHOOK_SECRET");
        if let Some(secret) = &webhook_secret {
            push_email::ResendWebhooks::new(secret)
                .map_err(|reason| redacted("PUSH_RESEND_WEBHOOK_SECRET", reason))?;
        }
        let poll_ms: u64 = parsed(&lookup, "PUSH_WORKER_POLL_MS", 1000)?;
        let unverified_days: u64 = parsed(&lookup, "PUSH_UNVERIFIED_ACCOUNT_DAYS", 7)?;
        let retention_days: u64 = parsed(&lookup, "PUSH_WORKER_RETENTION_DAYS", 30)?;
        for (name, value) in [
            ("PUSH_WORKER_POLL_MS", poll_ms),
            ("PUSH_UNVERIFIED_ACCOUNT_DAYS", unverified_days),
            ("PUSH_WORKER_RETENTION_DAYS", retention_days),
        ] {
            if value == 0 {
                return Err(ConfigError::Invalid {
                    name,
                    value: "0".to_owned(),
                    reason: "must be at least 1".to_owned(),
                });
            }
        }
        let dev_mailbox = dev_flag(&lookup, "PUSH_DEV_MAILBOX")?;
        if dev_mailbox && sender != Sender::Log {
            return Err(ConfigError::Invalid {
                name: "PUSH_DEV_MAILBOX",
                value: "1".to_owned(),
                reason: "the DEV ONLY mailbox shows every email it sent, links included; it \
                         is only allowed with PUSH_EMAIL_SENDER=log"
                    .to_owned(),
            });
        }
        Ok(Self {
            database_url,
            auth_database_url,
            message_svc_url,
            accounts_token,
            sender,
            from,
            product_name,
            webhook_secret,
            poll: Duration::from_millis(poll_ms),
            unverified_after: DAY.saturating_mul(unverified_days.min(36_500) as u32),
            retention: DAY.saturating_mul(retention_days.min(36_500) as u32),
            http_addr: parsed(&lookup, "PUSH_HTTP_ADDR", ([0, 0, 0, 0], 8080).into())?,
            health_addr: parsed(&lookup, "PUSH_HEALTH_ADDR", ([0, 0, 0, 0], 8081).into())?,
            store,
            dev_mailbox,
        })
    }
}

/// `http://127.0.0.1…` or `http://localhost…`: a stand-in for Resend in tests.
fn is_local_http(url: &str) -> bool {
    [
        "http://127.0.0.1:",
        "http://127.0.0.1/",
        "http://localhost:",
        "http://localhost/",
    ]
    .iter()
    .any(|prefix| url.starts_with(prefix))
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
/// else is refused, so a typo can never switch it on or leave it in doubt.
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

    const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";

    const TOKEN: &str = "accounts-token-of-the-config-tests";

    const REQUIRED: [(&str, &str); 4] = [
        (
            "PUSH_DATABASE_URL",
            "postgres://push_worker:dbsecret@db/push_worker",
        ),
        (
            "PUSH_AUTH_DATABASE_URL",
            "postgres://push_worker:dbsecret@db/push_auth",
        ),
        ("PUSH_MESSAGE_SVC_URL", "http://message-svc:50051"),
        ("PUSH_ACCOUNTS_TOKEN", TOKEN),
    ];

    fn with(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let mut variables = REQUIRED.to_vec();
        variables.retain(|(name, _)| !extra.iter().any(|(key, _)| key == name));
        variables.extend_from_slice(extra);
        variables
    }

    #[test]
    fn defaults_apply_when_only_the_required_variables_are_set() {
        let config = config(&REQUIRED).unwrap();
        assert_eq!(config.sender, Sender::Log);
        assert_eq!(config.accounts_token, TOKEN);
        assert_eq!(config.product_name, "Push Messenger");
        assert_eq!(config.webhook_secret, None);
        assert_eq!(config.poll, Duration::from_secs(1));
        assert_eq!(config.unverified_after, DAY * 7);
        assert_eq!(config.retention, DAY * 30);
        assert_eq!(config.http_addr, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(config.health_addr, "0.0.0.0:8081".parse().unwrap());
        assert_eq!(config.store.max_connections, 5);
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

    #[test]
    fn message_svc_is_reached_over_plain_http_with_a_header_safe_token() {
        for url in [
            "https://message-svc:50051",
            "message-svc:50051",
            "grpc://m:1",
        ] {
            assert!(
                matches!(
                    config(&with(&[("PUSH_MESSAGE_SVC_URL", url)])),
                    Err(ConfigError::Invalid {
                        name: "PUSH_MESSAGE_SVC_URL",
                        ..
                    })
                ),
                "{url}"
            );
        }
        for token in ["short", "a token with spaces, long enough otherwise"] {
            let message = config(&with(&[("PUSH_ACCOUNTS_TOKEN", token)]))
                .unwrap_err()
                .to_string();
            assert!(message.contains("PUSH_ACCOUNTS_TOKEN"), "{message}");
            assert!(!message.contains(token), "{message}");
        }
    }

    #[test]
    fn resend_needs_a_key_and_a_from_address() {
        let resend = config(&with(&[
            ("PUSH_EMAIL_SENDER", "resend"),
            ("PUSH_RESEND_API_KEY", "re_key"),
            ("PUSH_EMAIL_FROM", "Push <no-reply@example.com>"),
            ("PUSH_RESEND_WEBHOOK_SECRET", SECRET),
            ("PUSH_EMAIL_PRODUCT_NAME", "Acme Chat"),
        ]))
        .unwrap();
        assert_eq!(
            resend.sender,
            Sender::Resend {
                api_url: "https://api.resend.com".to_owned(),
                api_key: "re_key".to_owned(),
            }
        );
        assert_eq!(resend.from, "Push <no-reply@example.com>");
        assert_eq!(resend.product_name, "Acme Chat");
        assert_eq!(
            config(&with(&[
                ("PUSH_EMAIL_SENDER", "resend"),
                ("PUSH_EMAIL_FROM", "a@example.com")
            ])),
            Err(ConfigError::Missing("PUSH_RESEND_API_KEY"))
        );
        assert_eq!(
            config(&with(&[
                ("PUSH_EMAIL_SENDER", "resend"),
                ("PUSH_RESEND_API_KEY", "re_key")
            ])),
            Err(ConfigError::Missing("PUSH_EMAIL_FROM"))
        );
        for (name, value) in [
            ("PUSH_EMAIL_SENDER", "smtp"),
            ("PUSH_RESEND_API_URL", "http://api.example.com"),
        ] {
            let result = config(&with(&[
                ("PUSH_EMAIL_SENDER", "resend"),
                ("PUSH_RESEND_API_KEY", "re_key"),
                ("PUSH_EMAIL_FROM", "a@example.com"),
                (name, value),
            ]));
            assert!(
                matches!(result, Err(ConfigError::Invalid { name: n, .. }) if n == name),
                "{name}={value}"
            );
        }
        let local = config(&with(&[
            ("PUSH_EMAIL_SENDER", "resend"),
            ("PUSH_RESEND_API_KEY", "re_key"),
            ("PUSH_EMAIL_FROM", "a@example.com"),
            ("PUSH_RESEND_API_URL", "http://127.0.0.1:9000"),
        ]));
        assert!(local.is_ok(), "{local:?}");
    }

    #[test]
    fn the_product_name_has_no_control_characters() {
        for name in [
            "Acme\r\nBcc: victim@example.com",
            "Acme\nChat",
            "Acme\rChat",
            "Acme\tChat",
            "Acme\u{0}Chat",
            "Acme\u{7f}Chat",
        ] {
            let result = config(&with(&[("PUSH_EMAIL_PRODUCT_NAME", name)]));
            assert!(
                matches!(
                    result,
                    Err(ConfigError::Invalid {
                        name: "PUSH_EMAIL_PRODUCT_NAME",
                        ..
                    })
                ),
                "{name:?}"
            );
        }
        let accented = config(&with(&[("PUSH_EMAIL_PRODUCT_NAME", "Æble Chat")])).unwrap();
        assert_eq!(accented.product_name, "Æble Chat");
        assert!(matches!(
            config(&with(&[(
                "PUSH_EMAIL_FROM",
                "Push <no-reply@push.test>\r\nBcc: victim@example.com"
            )])),
            Err(ConfigError::Invalid {
                name: "PUSH_EMAIL_FROM",
                ..
            })
        ));
    }

    #[test]
    fn the_dev_mailbox_is_strict_and_only_for_the_log_sender() {
        for (value, expected) in [("1", true), ("0", false), ("", false)] {
            let config = config(&with(&[("PUSH_DEV_MAILBOX", value)])).unwrap();
            assert_eq!(config.dev_mailbox, expected, "{value:?}");
        }
        for value in ["true", "yes", " 1"] {
            assert!(config(&with(&[("PUSH_DEV_MAILBOX", value)])).is_err());
        }
        let with_resend = config(&with(&[
            ("PUSH_DEV_MAILBOX", "1"),
            ("PUSH_EMAIL_SENDER", "resend"),
            ("PUSH_RESEND_API_KEY", "re_key"),
            ("PUSH_EMAIL_FROM", "a@example.com"),
        ]));
        assert!(matches!(
            with_resend,
            Err(ConfigError::Invalid {
                name: "PUSH_DEV_MAILBOX",
                ..
            })
        ));
    }

    #[test]
    fn periods_must_be_at_least_one() {
        for name in [
            "PUSH_WORKER_POLL_MS",
            "PUSH_UNVERIFIED_ACCOUNT_DAYS",
            "PUSH_WORKER_RETENTION_DAYS",
        ] {
            assert!(config(&with(&[(name, "0")])).is_err(), "{name}");
        }
        let config = config(&with(&[
            ("PUSH_WORKER_POLL_MS", "250"),
            ("PUSH_UNVERIFIED_ACCOUNT_DAYS", "2"),
            ("PUSH_WORKER_RETENTION_DAYS", "90"),
        ]))
        .unwrap();
        assert_eq!(config.poll, Duration::from_millis(250));
        assert_eq!(config.unverified_after, DAY * 2);
        assert_eq!(config.retention, DAY * 90);
    }

    #[test]
    fn secrets_never_show_in_errors_or_debug_output() {
        let message = config(&with(&[(
            "PUSH_AUTH_DATABASE_URL",
            "mysql://push:hunter2@db/x",
        )]))
        .unwrap_err()
        .to_string();
        assert!(message.contains("PUSH_AUTH_DATABASE_URL") && !message.contains("hunter2"));
        let message = config(&with(&[("PUSH_RESEND_WEBHOOK_SECRET", "whsec_hunter2!")]))
            .unwrap_err()
            .to_string();
        assert!(message.contains("PUSH_RESEND_WEBHOOK_SECRET") && !message.contains("hunter2"));
        let shown = format!(
            "{:?}",
            config(&with(&[
                ("PUSH_EMAIL_SENDER", "resend"),
                ("PUSH_RESEND_API_KEY", "re_hunter2"),
                ("PUSH_EMAIL_FROM", "a@example.com"),
                ("PUSH_RESEND_WEBHOOK_SECRET", SECRET),
            ]))
            .unwrap()
        );
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(!shown.contains("MfKQ"), "{shown}");
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains("http://message-svc:50051"), "{shown}");
    }
}
