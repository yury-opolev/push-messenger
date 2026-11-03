//! TURN credentials for voice calls (spec 2026-11-03 voice calls §8, T5, T6, T12):
//! `GET /v1/calls/ice-servers` hands a signed-in user the browser-ready ICE servers and
//! when they expire.
//!
//! - `internal` (the chart's coturn) and `external` `generic` use the TURN REST scheme:
//!   username `"<expiry unix seconds>:<user id>"`, credential
//!   `base64(HMAC-SHA1(shared secret, username))`, which coturn checks with
//!   `use-auth-secret`.
//! - `external` `cloudflare` and `twilio` ask the provider for short-lived credentials
//!   (`provider`) and keep them per user until shortly before they expire; concurrent
//!   requests of one user share one provider call.
//! - `stun-only` hands out STUN servers only (`relay: false`), for development.
//!
//! Without TURN settings, or when the provider fails or answers with nothing usable, the
//! answer is `turn_unavailable`: never servers a client could not use. Secrets and
//! credentials are never logged: their `Debug` output is redacted.

mod provider;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, TimeDelta, Utc};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use uuid::Uuid;

use crate::config::{ConfigError, problem, redacted};
use crate::error::AuthError;

pub use provider::{CLOUDFLARE_BASE_URL, TWILIO_BASE_URL};

/// Shortest and longest credential lifetime, and the default (`calls.turn.ttlSeconds`).
pub const MIN_TTL_SECONDS: u32 = 60;
pub const MAX_TTL_SECONDS: u32 = 86_400;
pub const DEFAULT_TTL_SECONDS: u32 = 600;
/// Most URLs in one list variable, and the longest URL.
const MAX_URLS: usize = 16;
const MAX_URL_BYTES: usize = 512;
/// Shortest TURN REST shared secret.
const MIN_SECRET_BYTES: usize = 16;
/// Most users with provider credentials kept in memory.
const CACHE_CAPACITY: usize = 10_000;

/// A secret value: compared, never shown.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Where credentials come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The TURN REST scheme with a shared secret: the chart's coturn (`internal`) or a
    /// provider's (`external` `generic`).
    Rest {
        internal: bool,
        urls: Vec<String>,
        secret: Secret,
    },
    /// Cloudflare Realtime TURN: minted through its API with the TURN key's API token.
    Cloudflare { key_id: String, token: Secret },
    /// Twilio Network Traversal Service tokens, with the account's auth token.
    Twilio { account_sid: String, token: Secret },
    /// STUN only: no relay.
    StunOnly,
}

/// The TURN settings of auth-svc (`PUSH_TURN_*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSettings {
    pub ttl_seconds: u32,
    /// STUN servers listed first in every answer.
    pub stun_urls: Vec<String>,
    pub source: Source,
}

impl TurnSettings {
    /// The `mode` label of the metrics: `internal`, `generic`, `cloudflare`, `twilio` or
    /// `stun-only`.
    pub fn mode(&self) -> &'static str {
        match self.source {
            Source::Rest { internal: true, .. } => "internal",
            Source::Rest {
                internal: false, ..
            } => "generic",
            Source::Cloudflare { .. } => "cloudflare",
            Source::Twilio { .. } => "twilio",
            Source::StunOnly => "stun-only",
        }
    }

    /// Reads the `PUSH_TURN_*` variables; `None` without `PUSH_TURN_MODE` (no TURN: the
    /// API answers `turn_unavailable`).
    pub(crate) fn from_lookup(
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, ConfigError> {
        let value = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let Some(mode) = value("PUSH_TURN_MODE") else {
            return Ok(None);
        };
        let ttl_seconds = match value("PUSH_TURN_TTL_SECONDS") {
            None => DEFAULT_TTL_SECONDS,
            Some(text) => text
                .parse::<u32>()
                .ok()
                .filter(|ttl| (MIN_TTL_SECONDS..=MAX_TTL_SECONDS).contains(ttl))
                .ok_or_else(|| ConfigError::Invalid {
                    name: "PUSH_TURN_TTL_SECONDS",
                    value: text.clone(),
                    reason: "use a whole number of seconds from 60 to 86400".to_owned(),
                })?,
        };
        let stun_urls = url_list(
            value("PUSH_TURN_STUN_URLS"),
            "PUSH_TURN_STUN_URLS",
            &["stun:", "stuns:"],
        )?;
        let source = match mode.as_str() {
            "internal" => rest(lookup, &value, true)?,
            "external" => match value("PUSH_TURN_PROVIDER").as_deref().unwrap_or("generic") {
                "generic" => rest(lookup, &value, false)?,
                "cloudflare" => {
                    let key_id = value("PUSH_TURN_CLOUDFLARE_KEY_ID")
                        .ok_or(ConfigError::Missing("PUSH_TURN_CLOUDFLARE_KEY_ID"))?;
                    if key_id.len() > 64 || !key_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
                        return Err(ConfigError::Invalid {
                            name: "PUSH_TURN_CLOUDFLARE_KEY_ID",
                            value: key_id,
                            reason: "the TURN key id: 1 to 64 letters and digits".to_owned(),
                        });
                    }
                    Source::Cloudflare {
                        key_id,
                        token: token(lookup, "PUSH_TURN_CLOUDFLARE_TOKEN")?,
                    }
                }
                "twilio" => {
                    let account_sid = value("PUSH_TURN_TWILIO_ACCOUNT_SID")
                        .ok_or(ConfigError::Missing("PUSH_TURN_TWILIO_ACCOUNT_SID"))?;
                    if !(account_sid.len() == 34
                        && account_sid.starts_with("AC")
                        && account_sid[2..].bytes().all(|b| b.is_ascii_hexdigit()))
                    {
                        return Err(ConfigError::Invalid {
                            name: "PUSH_TURN_TWILIO_ACCOUNT_SID",
                            value: account_sid,
                            reason: "AC followed by 32 hexadecimal digits".to_owned(),
                        });
                    }
                    Source::Twilio {
                        account_sid,
                        token: token(lookup, "PUSH_TURN_TWILIO_TOKEN")?,
                    }
                }
                other => {
                    return Err(ConfigError::Invalid {
                        name: "PUSH_TURN_PROVIDER",
                        value: other.to_owned(),
                        reason: "expected generic, cloudflare or twilio".to_owned(),
                    });
                }
            },
            "stun-only" => {
                if stun_urls.is_empty() {
                    return Err(ConfigError::Missing("PUSH_TURN_STUN_URLS"));
                }
                Source::StunOnly
            }
            other => {
                return Err(ConfigError::Invalid {
                    name: "PUSH_TURN_MODE",
                    value: other.to_owned(),
                    reason: "expected internal, external or stun-only".to_owned(),
                });
            }
        };
        Ok(Some(Self {
            ttl_seconds,
            stun_urls,
            source,
        }))
    }
}

/// The TURN REST source: `PUSH_TURN_URLS` and the shared secret.
fn rest(
    lookup: &dyn Fn(&str) -> Option<String>,
    value: &dyn Fn(&str) -> Option<String>,
    internal: bool,
) -> Result<Source, ConfigError> {
    let urls = url_list(
        value("PUSH_TURN_URLS"),
        "PUSH_TURN_URLS",
        &["turn:", "turns:"],
    )?;
    if urls.is_empty() {
        return Err(ConfigError::Missing("PUSH_TURN_URLS"));
    }
    let secret = token(lookup, "PUSH_TURN_SHARED_SECRET")?;
    if secret.expose().len() < MIN_SECRET_BYTES {
        return Err(redacted(
            "PUSH_TURN_SHARED_SECRET",
            "use at least 16 visible ASCII characters".to_owned(),
        ));
    }
    Ok(Source::Rest {
        internal,
        urls,
        secret,
    })
}

/// A required secret of visible ASCII characters (also from `<name>_FILE`), without the
/// line break a mounted file may end with.
fn token(
    lookup: &dyn Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<Secret, ConfigError> {
    let value = push_credentials::required_application_secret(lookup, name).map_err(problem)?;
    let value = value.trim_end_matches(['\r', '\n']);
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(redacted(
            name,
            "use visible ASCII characters only".to_owned(),
        ));
    }
    Ok(Secret::new(value))
}

/// Comma-separated URLs, each with one of `schemes`.
fn url_list(
    value: Option<String>,
    name: &'static str,
    schemes: &[&str],
) -> Result<Vec<String>, ConfigError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let urls: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .collect();
    let invalid = |reason: String| ConfigError::Invalid {
        name,
        value: value.clone(),
        reason,
    };
    if urls.len() > MAX_URLS {
        return Err(invalid(format!("at most {MAX_URLS} URLs")));
    }
    for url in &urls {
        let scheme = schemes.iter().find(|scheme| {
            url.get(..scheme.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
        });
        let has_host = scheme.is_some_and(|scheme| url.len() > scheme.len());
        if !has_host || url.len() > MAX_URL_BYTES || !url.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(invalid(format!(
                "comma-separated {} URLs of at most {MAX_URL_BYTES} visible ASCII characters",
                schemes.join(" or ")
            )));
        }
    }
    Ok(urls)
}

/// One entry of `RTCConfiguration.iceServers`.
#[derive(Clone, PartialEq, Eq, serde::Serialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

impl std::fmt::Debug for IceServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hidden = |value: &Option<String>| value.as_ref().map(|_| "<redacted>");
        formatter
            .debug_struct("IceServer")
            .field("urls", &self.urls)
            .field("username", &hidden(&self.username))
            .field("credential", &hidden(&self.credential))
            .finish()
    }
}

/// The answer of `GET /v1/calls/ice-servers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceServers {
    pub servers: Vec<IceServer>,
    /// When the credentials stop working; ask again before.
    pub expires_at: DateTime<Utc>,
    /// Whether a relay (TURN) is among the servers.
    pub relay: bool,
}

/// The TURN REST username of `user` for credentials expiring at `expires_at_unix`.
pub fn rest_username(expires_at_unix: i64, user: Uuid) -> String {
    format!("{expires_at_unix}:{user}")
}

/// The TURN REST credential of `username`: `base64(HMAC-SHA1(secret, username))`, what
/// coturn computes with `use-auth-secret` and `static-auth-secret`.
pub fn rest_credential(secret: &str, username: &str) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(username.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

/// Provider credentials kept for one user.
#[derive(Debug, Clone)]
struct Minted {
    servers: Vec<IceServer>,
    expires_at: DateTime<Utc>,
}

type Slot = Arc<tokio::sync::Mutex<Option<Minted>>>;
type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// The credential counters of `GET /metrics` (spec §8).
#[derive(Debug, Default)]
struct TurnMetrics {
    issued: AtomicU64,
    failures: AtomicU64,
    provider_ok: AtomicU64,
    provider_errors: AtomicU64,
}

/// Mints ICE servers for users.
pub struct Turn {
    settings: Option<TurnSettings>,
    provider: Option<provider::Provider>,
    cache: Mutex<HashMap<Uuid, Slot>>,
    metrics: TurnMetrics,
    clock: Clock,
}

impl std::fmt::Debug for Turn {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Turn")
            .field("mode", &self.mode())
            .finish_non_exhaustive()
    }
}

impl Turn {
    /// No TURN settings: every request is answered `turn_unavailable`.
    pub fn disabled() -> Self {
        Self {
            settings: None,
            provider: None,
            cache: Mutex::default(),
            metrics: TurnMetrics::default(),
            clock: Arc::new(Utc::now),
        }
    }

    /// Minting with `settings` (none: [`Turn::disabled`]); providers are called at their
    /// public URLs.
    pub fn new(settings: Option<TurnSettings>) -> Result<Self, String> {
        let provider = match settings.as_ref().map(|settings| &settings.source) {
            Some(Source::Cloudflare { .. }) => Some(provider::Provider::new(CLOUDFLARE_BASE_URL)?),
            Some(Source::Twilio { .. }) => Some(provider::Provider::new(TWILIO_BASE_URL)?),
            _ => None,
        };
        Ok(Self {
            settings,
            provider,
            ..Self::disabled()
        })
    }

    /// Calls the provider at `base_url` instead (a stub in tests).
    #[doc(hidden)]
    pub fn with_provider_base_url(mut self, base_url: &str) -> Result<Self, String> {
        if self.provider.is_some() {
            self.provider = Some(provider::Provider::new(base_url)?);
        }
        Ok(self)
    }

    #[cfg(test)]
    fn with_clock(mut self, clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// The configured mode, if any.
    pub fn mode(&self) -> Option<&'static str> {
        self.settings.as_ref().map(TurnSettings::mode)
    }

    /// The ICE servers for `user`, or `TurnUnavailable`.
    pub async fn ice_servers(&self, user: Uuid) -> Result<IceServers, AuthError> {
        let Some(settings) = &self.settings else {
            return Err(AuthError::TurnUnavailable);
        };
        let result = self.mint(settings, user).await;
        let counter = match result {
            Ok(_) => &self.metrics.issued,
            Err(_) => &self.metrics.failures,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        result
    }

    async fn mint(&self, settings: &TurnSettings, user: Uuid) -> Result<IceServers, AuthError> {
        let now = (self.clock)();
        let mut servers = Vec::new();
        if !settings.stun_urls.is_empty() {
            servers.push(IceServer {
                urls: settings.stun_urls.clone(),
                username: None,
                credential: None,
            });
        }
        let ttl = TimeDelta::seconds(i64::from(settings.ttl_seconds));
        let (relay, expires_at) = match &settings.source {
            Source::Rest { urls, secret, .. } => {
                let expires_at = whole_seconds(now + ttl);
                let username = rest_username(expires_at.timestamp(), user);
                let credential = rest_credential(secret.expose(), &username);
                servers.push(IceServer {
                    urls: urls.clone(),
                    username: Some(username),
                    credential: Some(credential),
                });
                (true, expires_at)
            }
            Source::StunOnly => (false, whole_seconds(now + ttl)),
            Source::Cloudflare { .. } | Source::Twilio { .. } => {
                let minted = self.minted(settings, user).await?;
                servers.extend(minted.servers);
                (true, minted.expires_at)
            }
        };
        Ok(IceServers {
            servers,
            expires_at,
            relay,
        })
    }

    /// The provider credentials of `user`: kept ones while they have more than the
    /// refresh margin left, else new ones. One provider call per user at a time.
    async fn minted(&self, settings: &TurnSettings, user: Uuid) -> Result<Minted, AuthError> {
        let margin = refresh_margin(settings.ttl_seconds);
        let slot = {
            let mut cache = self.cache.lock().expect("not poisoned");
            if cache.len() >= CACHE_CAPACITY && !cache.contains_key(&user) {
                let now = (self.clock)();
                // Expired entries go first; when all are fresh, every idle one.
                cache.retain(|_, slot| {
                    slot.try_lock()
                        .map(|kept| kept.as_ref().is_some_and(|m| m.expires_at - margin > now))
                        .unwrap_or(true)
                });
                if cache.len() >= CACHE_CAPACITY {
                    cache.retain(|_, slot| slot.try_lock().is_err());
                }
            }
            Arc::clone(cache.entry(user).or_default())
        };
        let mut kept = slot.lock().await;
        let now = (self.clock)();
        if let Some(minted) = kept.as_ref() {
            if minted.expires_at - margin > now {
                return Ok(minted.clone());
            }
        }
        let provider = self.provider.as_ref().ok_or(AuthError::TurnUnavailable)?;
        match provider.mint(settings, now).await {
            Ok((servers, expires_at)) => {
                self.metrics.provider_ok.fetch_add(1, Ordering::Relaxed);
                let minted = Minted {
                    servers,
                    expires_at: whole_seconds(expires_at),
                };
                *kept = Some(minted.clone());
                Ok(minted)
            }
            Err(error) => {
                self.metrics.provider_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    provider = settings.mode(),
                    %error,
                    "the TURN provider issued no usable credentials"
                );
                Err(AuthError::TurnUnavailable)
            }
        }
    }

    /// The metrics in the Prometheus text format 0.0.4; empty without TURN settings.
    /// Labels are the mode, provider and outcome only.
    pub fn render_metrics(&self) -> String {
        let Some(settings) = &self.settings else {
            return String::new();
        };
        let mode = settings.mode();
        let mut out = String::new();
        let mut family = |name: &str, help: &str, samples: &[(String, &AtomicU64)]| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
            for (labels, value) in samples {
                let _ = writeln!(out, "{name}{{{labels}}} {}", value.load(Ordering::Relaxed));
            }
        };
        let labels = format!("mode=\"{mode}\"");
        family(
            "push_turn_credentials_issued_total",
            "ICE server lists handed out by GET /v1/calls/ice-servers.",
            &[(labels.clone(), &self.metrics.issued)],
        );
        family(
            "push_turn_credentials_failures_total",
            "ICE server requests answered turn_unavailable.",
            &[(labels, &self.metrics.failures)],
        );
        if self.provider.is_some() {
            family(
                "push_turn_provider_requests_total",
                "Credential requests to the TURN provider.",
                &[
                    (
                        format!("provider=\"{mode}\",outcome=\"ok\""),
                        &self.metrics.provider_ok,
                    ),
                    (
                        format!("provider=\"{mode}\",outcome=\"error\""),
                        &self.metrics.provider_errors,
                    ),
                ],
            );
        }
        out
    }
}

/// Credentials are refreshed this long before they expire: a minute, or a fifth of a
/// short lifetime.
fn refresh_margin(ttl_seconds: u32) -> TimeDelta {
    TimeDelta::seconds(i64::from((ttl_seconds / 5).min(60)))
}

fn whole_seconds(at: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp(at.timestamp(), 0).unwrap_or(at)
}
