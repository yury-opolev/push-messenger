//! Redis with a password URL, or with Entra ID tokens (spec 2026-10-18 §6): TLS
//! (`rediss://`), the token's `oid` (or `PUSH_REDIS_USER`) as the ACL user, managed
//! connections that re-`AUTH` whenever the token changes, and subscribers opened with the
//! current token (the gateway hub replaces them when the token changes).

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::Stream;
use redis::aio::{ConnectionManager, ConnectionManagerConfig, PubSub};
use redis::auth::{BasicAuth, StreamingCredentialsProvider};
use redis::{ErrorKind, RedisError, RedisResult};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{AccessToken, AuthMode, ConfigProblem, TokenError, TokenSource, Tokens};

/// The scope of Azure Managed Redis (and Azure Cache for Redis) tokens.
pub const REDIS_SCOPE: &str = "https://redis.azure.com/.default";

/// Where Redis is and how to sign in. `Debug` never shows a password URL.
#[derive(Clone, PartialEq, Eq)]
pub enum RedisTarget {
    /// `redis://:password@host:6379` (or `rediss://`), as before.
    Password { url: String },
    /// `rediss://host:port` without credentials; the user is `user` or the token's `oid`.
    Entra {
        url: String,
        user: Option<String>,
        ca_file: Option<PathBuf>,
    },
}

impl std::fmt::Debug for RedisTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Password { .. } => formatter.write_str("Password { url: <redacted> }"),
            Self::Entra { url, user, ca_file } => formatter
                .debug_struct("Entra")
                .field("url", url)
                .field("user", user)
                .field("ca_file", ca_file)
                .finish(),
        }
    }
}

impl RedisTarget {
    /// Reads `PUSH_REDIS_AUTH`, `PUSH_REDIS_URL`, `PUSH_REDIS_USER`, `PUSH_REDIS_CA_FILE`.
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self, ConfigProblem> {
        let get = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let mode = AuthMode::parse(lookup("PUSH_REDIS_AUTH").as_deref()).map_err(|reason| {
            ConfigProblem::Invalid {
                name: "PUSH_REDIS_AUTH",
                reason,
            }
        })?;
        let url = get("PUSH_REDIS_URL").ok_or(ConfigProblem::Missing("PUSH_REDIS_URL"))?;
        let invalid = |reason: &str| ConfigProblem::Invalid {
            name: "PUSH_REDIS_URL",
            reason: reason.to_owned(),
        };
        match mode {
            AuthMode::Password => {
                redis::Client::open(url.as_str()).map_err(|_| {
                    invalid("not a valid Redis URL, expected redis://[:password@]host:6379")
                })?;
                Ok(Self::Password { url })
            }
            AuthMode::AzureEntra => {
                let parsed = reqwest::Url::parse(&url).map_err(|_| invalid("not a valid URL"))?;
                if parsed.scheme() != "rediss" {
                    return Err(invalid("azure-entra needs a rediss:// URL (TLS)"));
                }
                if !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || parsed.host_str().is_none()
                {
                    return Err(invalid(
                        "azure-entra takes rediss://host:port without credentials",
                    ));
                }
                redis::Client::open(url.as_str()).map_err(|_| invalid("not a valid Redis URL"))?;
                Ok(Self::Entra {
                    url,
                    user: get("PUSH_REDIS_USER"),
                    ca_file: get("PUSH_REDIS_CA_FILE").map(PathBuf::from),
                })
            }
        }
    }

    pub fn mode(&self) -> AuthMode {
        match self {
            Self::Password { .. } => AuthMode::Password,
            Self::Entra { .. } => AuthMode::AzureEntra,
        }
    }
}

/// Why a connector could not be opened. Never retried: it is configuration or identity.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("{0}")]
    Config(String),
    #[error("cannot get an access token for Redis: {0}")]
    Token(#[from] TokenError),
}

/// Opens Redis connections with the target's credentials. Cheap to clone.
#[derive(Clone)]
pub struct RedisConnector {
    client: redis::Client,
    credentials: Option<Credentials>,
}

impl RedisConnector {
    /// Password mode from a URL.
    pub fn password(url: &str) -> RedisResult<Self> {
        Ok(Self {
            client: redis::Client::open(url)?,
            credentials: None,
        })
    }

    /// The connector for `target`; in Entra mode it fetches the first token from
    /// `azure` (failing when none can be had) and keeps it fresh until `shutdown`.
    pub async fn open(
        target: &RedisTarget,
        azure: Option<&Arc<dyn TokenSource>>,
        shutdown: CancellationToken,
    ) -> Result<Self, OpenError> {
        match target {
            RedisTarget::Password { url } => Self::password(url).map_err(|_| {
                OpenError::Config("PUSH_REDIS_URL is not a valid Redis URL".to_owned())
            }),
            RedisTarget::Entra { url, user, ca_file } => {
                let source = azure.ok_or_else(|| {
                    OpenError::Config(
                        "PUSH_REDIS_AUTH=azure-entra needs the AZURE_* identity variables"
                            .to_owned(),
                    )
                })?;
                let root_cert = match ca_file {
                    Some(file) => Some(tokio::fs::read(file).await.map_err(|error| {
                        OpenError::Config(format!(
                            "cannot read PUSH_REDIS_CA_FILE: {}",
                            error.kind()
                        ))
                    })?),
                    None => None,
                };
                let client = match root_cert {
                    Some(pem) => redis::Client::build_with_tls(
                        url.as_str(),
                        redis::TlsCertificates {
                            client_tls: None,
                            root_cert: Some(pem),
                        },
                    ),
                    None => redis::Client::open(url.as_str()),
                }
                .map_err(|_| {
                    OpenError::Config("PUSH_REDIS_URL is not a valid Redis URL".to_owned())
                })?;
                let tokens = Tokens::start(Arc::clone(source), REDIS_SCOPE, shutdown).await?;
                Ok(Self {
                    client,
                    credentials: Some(Credentials {
                        tokens,
                        user: user.as_deref().map(Arc::from),
                    }),
                })
            }
        }
    }

    fn with_credentials(&self, config: ConnectionManagerConfig) -> ConnectionManagerConfig {
        match &self.credentials {
            Some(credentials) => config.set_credentials_provider(credentials.clone()),
            None => config,
        }
    }

    /// A self-reconnecting connection; in Entra mode it re-authenticates on every token.
    pub async fn connection_manager(
        &self,
        config: ConnectionManagerConfig,
    ) -> RedisResult<ConnectionManager> {
        self.client
            .get_connection_manager_with_config(self.with_credentials(config))
            .await
    }

    /// As [`Self::connection_manager`], connecting on first use.
    pub fn connection_manager_lazy(
        &self,
        config: ConnectionManagerConfig,
    ) -> RedisResult<ConnectionManager> {
        self.client
            .get_connection_manager_lazy(self.with_credentials(config))
    }

    /// A pub/sub connection signed in with the current token. A subscribed connection
    /// cannot re-authenticate: replace it when [`Self::rotations`] fires.
    pub async fn pubsub(&self) -> RedisResult<PubSub> {
        let Some(credentials) = &self.credentials else {
            return self.client.get_async_pubsub().await;
        };
        let auth = credentials.basic_auth(&credentials.tokens.current())?;
        let info = self.client.get_connection_info().clone();
        let settings = info
            .redis_settings()
            .clone()
            .set_username(auth.username())
            .set_password(auth.password());
        // The address keeps its TLS parameters (the CA), so `open` reuses them.
        redis::Client::open(info.set_redis_settings(settings))?
            .get_async_pubsub()
            .await
    }

    /// Fires on every new token in Entra mode; `None` in password mode.
    pub fn rotations(&self) -> Option<watch::Receiver<AccessToken>> {
        self.credentials
            .as_ref()
            .map(|credentials| credentials.tokens.watch())
    }
}

/// Whether Redis refused the credentials (a wrong or expired token, an unknown user):
/// configuration, not a passing outage.
pub fn is_refused(error: &RedisError) -> bool {
    error.kind() == ErrorKind::AuthenticationFailed
        || matches!(error.code(), Some("WRONGPASS" | "NOPERM" | "NOAUTH"))
}

/// A Redis error that waiting will not fix in Entra mode: refused credentials or a
/// rejected server certificate. Everything else (refused connections, timeouts, resets)
/// is an outage and retried.
///
/// redis 1.2.2 keeps a TLS failure as an `io::Error` behind an `Arc<dyn Error>`, whose
/// `source()` skips the rustls error itself, so a rejected certificate is recognised by
/// rustls's own messages for its two certificate errors.
pub fn is_permanent_redis(error: &RedisError) -> bool {
    if is_refused(error) || crate::is_certificate_error(error) {
        return true;
    }
    let text = error.to_string();
    error.kind() == ErrorKind::Io
        && (text.contains("invalid peer certificate") || text.contains("peer sent no certificates"))
}

#[derive(Clone)]
struct Credentials {
    tokens: Tokens,
    user: Option<Arc<str>>,
}

impl Credentials {
    fn basic_auth(&self, token: &AccessToken) -> RedisResult<BasicAuth> {
        let user = match &self.user {
            Some(user) => user.to_string(),
            None => token.object_id().ok_or_else(|| {
                RedisError::from((
                    ErrorKind::AuthenticationFailed,
                    "no Redis user: set PUSH_REDIS_USER (the token has no oid claim)",
                ))
            })?,
        };
        Ok(BasicAuth::new(user, token.secret().to_owned()))
    }
}

impl StreamingCredentialsProvider for Credentials {
    /// The current credentials at once, then every new token's.
    fn subscribe(&self) -> Pin<Box<dyn Stream<Item = RedisResult<BasicAuth>> + Send + 'static>> {
        let mut receiver = self.tokens.watch();
        receiver.mark_changed();
        let credentials = self.clone();
        Box::pin(futures_util::stream::unfold(
            receiver,
            move |mut receiver| {
                let credentials = credentials.clone();
                async move {
                    receiver.changed().await.ok()?;
                    let token = receiver.borrow_and_update().clone();
                    Some((credentials.basic_auth(&token), receiver))
                }
            },
        ))
    }
}
