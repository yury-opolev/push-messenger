//! The signing keys of each key source (spec D4): one per listed tenant, or the one shared
//! endpoint of `common`, `organizations` or `consumers`, so the cache is bounded whatever
//! `tid`s tokens carry. A source's OpenID metadata names its issuer (checked for a listed
//! tenant; a keyword's names a template, and every token's issuer is checked against its
//! own `tid` anyway) and its key set, which must be on the authority host. Keys are fetched
//! on first use (and warmed at start-up), kept for `max_age`, fetched again for a token
//! naming an unknown key at most every `unknown_kid_cooldown`, and kept up to `stale_for`
//! longer while the authority is down. Without usable keys every Entra sign-in fails
//! closed. A source's fetches are serialised by its lock, so a burst of sign-ins fetches
//! once.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::DecodingKey;
use reqwest::Url;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use super::{EntraError, EntraSettings, Tenants};

/// Largest metadata or key set read.
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
/// Most keys taken from one key set.
const MAX_KEYS: usize = 64;
/// Longest wait for a connection.
const MAX_CONNECT: Duration = Duration::from_secs(5);

/// How keys are kept and fetched. Tests shorten these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyTiming {
    /// Keys older than this are fetched again before use.
    pub max_age: Duration,
    /// While fetching fails, keys up to `max_age + stale_for` old are still used.
    pub stale_for: Duration,
    /// A token naming an unknown key fetches the keys again at most this often.
    pub unknown_kid_cooldown: Duration,
    /// After a failed fetch, the next one waits this long; sign-ins fail meanwhile unless
    /// cached keys are still usable.
    pub retry_after_failure: Duration,
    /// Each request's limit, the answer included.
    pub fetch_timeout: Duration,
}

impl Default for KeyTiming {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(24 * 3600),
            stale_for: Duration::from_secs(24 * 3600),
            unknown_kid_cooldown: Duration::from_secs(300),
            retry_after_failure: Duration::from_secs(10),
            fetch_timeout: Duration::from_secs(10),
        }
    }
}

/// The keys of every key source.
pub(crate) struct KeySets {
    http: reqwest::Client,
    authority: Url,
    timing: KeyTiming,
    tenants: Tenants,
    sources: HashMap<String, Source>,
}

struct Source {
    /// The issuer its metadata must name: a listed tenant's; `None` for a keyword's.
    issuer: Option<String>,
    metadata_url: String,
    cache: tokio::sync::Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Option<Instant>,
    tried_at: Option<Instant>,
    last_error: Option<String>,
}

#[derive(Deserialize)]
struct Metadata {
    issuer: String,
    jwks_uri: String,
}

#[derive(Deserialize)]
struct KeySet {
    #[serde(default)]
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    #[serde(default)]
    kty: String,
    #[serde(default, rename = "use")]
    usage: Option<String>,
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
}

impl KeySets {
    pub(crate) fn new(settings: &EntraSettings, timing: KeyTiming) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(timing.fetch_timeout.min(MAX_CONNECT))
            .timeout(timing.fetch_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "cannot build the HTTP client for Entra ID".to_owned())?;
        let authority = Url::parse(&settings.authority_host)
            .map_err(|_| "the Entra ID authority host is not a URL".to_owned())?;
        let sources = settings
            .tenants
            .key_sources()
            .into_iter()
            .map(|source| {
                let entry = Source {
                    issuer: Uuid::try_parse(&source)
                        .ok()
                        .map(|tenant| settings.issuer(tenant)),
                    metadata_url: settings.metadata_url(&source),
                    cache: tokio::sync::Mutex::new(Cache::default()),
                };
                (source, entry)
            })
            .collect();
        Ok(Self {
            http,
            authority,
            timing,
            tenants: settings.tenants.clone(),
            sources,
        })
    }

    /// Fetches every source's keys that are not cached yet. Failures are logged, not
    /// fatal: readiness never depends on Entra ID.
    pub(crate) async fn warm(&self) {
        for (source, entry) in &self.sources {
            let mut cache = entry.cache.lock().await;
            if cache.fetched_at.is_some() {
                continue;
            }
            cache.tried_at = Some(Instant::now());
            match self.refresh(entry).await {
                Ok(keys) => {
                    cache.keys = keys;
                    cache.fetched_at = Some(Instant::now());
                    tracing::info!(%source, "fetched the Entra ID signing keys");
                }
                Err(error) => {
                    tracing::warn!(%source, %error, "cannot fetch the Entra ID signing keys yet; sign-ins try again");
                    cache.last_error = Some(error);
                }
            }
        }
    }

    /// The key `kid` for tokens of tenant `tenant`, from its key source; `None` if the
    /// tenant is not admitted or the source does not publish that key. `Unavailable` when
    /// no usable keys can be had.
    pub(crate) async fn key(
        &self,
        tenant: Uuid,
        kid: &str,
    ) -> Result<Option<DecodingKey>, EntraError> {
        let Some(source) = self.tenants.key_source(tenant) else {
            return Ok(None);
        };
        let Some(entry) = self.sources.get(&source) else {
            return Ok(None);
        };
        let mut cache = entry.cache.lock().await;
        let now = Instant::now();
        let fresh = cache
            .fetched_at
            .is_some_and(|at| now.duration_since(at) < self.timing.max_age);
        let wait = if fresh {
            self.timing.unknown_kid_cooldown
        } else {
            self.timing.retry_after_failure
        };
        let due = cache
            .tried_at
            .is_none_or(|at| now.duration_since(at) >= wait);
        if due && (!fresh || !cache.keys.contains_key(kid)) {
            cache.tried_at = Some(now);
            match self.refresh(entry).await {
                Ok(keys) => {
                    cache.keys = keys;
                    cache.fetched_at = Some(Instant::now());
                    cache.last_error = None;
                    return Ok(cache.keys.get(kid).cloned());
                }
                Err(error) => {
                    tracing::warn!(%source, %error, "cannot fetch the Entra ID signing keys");
                    cache.last_error = Some(error);
                }
            }
        }
        let usable = cache
            .fetched_at
            .is_some_and(|at| now.duration_since(at) < self.timing.max_age + self.timing.stale_for);
        if !usable {
            let reason = cache
                .last_error
                .clone()
                .unwrap_or_else(|| "no current signing keys".to_owned());
            return Err(EntraError::Unavailable(reason));
        }
        Ok(cache.keys.get(kid).cloned())
    }

    /// The source's RSA signing keys, by key id.
    async fn refresh(&self, source: &Source) -> Result<HashMap<String, DecodingKey>, String> {
        let metadata: Metadata = self.document(&source.metadata_url).await?;
        if source
            .issuer
            .as_ref()
            .is_some_and(|issuer| metadata.issuer != *issuer)
        {
            return Err("the metadata names another issuer".to_owned());
        }
        let jwks_url = self.on_authority(&metadata.jwks_uri)?;
        let set: KeySet = self.document(jwks_url.as_str()).await?;
        let keys: HashMap<String, DecodingKey> = set
            .keys
            .into_iter()
            .take(MAX_KEYS)
            .filter(|key| {
                key.kty == "RSA" && key.usage.as_deref().is_none_or(|usage| usage == "sig")
            })
            .filter_map(|key| {
                let decoding =
                    DecodingKey::from_rsa_components(key.n.as_deref()?, key.e.as_deref()?).ok()?;
                Some((key.kid?, decoding))
            })
            .collect();
        if keys.is_empty() {
            return Err("the key set has no RSA signing key".to_owned());
        }
        Ok(keys)
    }

    /// `url` if it is on the authority's scheme, host and port: egress stays one host.
    fn on_authority(&self, url: &str) -> Result<Url, String> {
        let url = Url::parse(url).map_err(|_| "the metadata's jwks_uri is not a URL".to_owned())?;
        let same = url.scheme() == self.authority.scheme()
            && url.host_str() == self.authority.host_str()
            && url.port_or_known_default() == self.authority.port_or_known_default()
            && url.username().is_empty()
            && url.password().is_none();
        if !same {
            return Err("the metadata's jwks_uri is not on the authority host".to_owned());
        }
        Ok(url)
    }

    /// A JSON document, read up to [`MAX_DOCUMENT_BYTES`].
    async fn document<T: DeserializeOwned>(&self, url: &str) -> Result<T, String> {
        let mut response = self.http.get(url).send().await.map_err(describe)?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(describe)? {
            if body.len() + chunk.len() > MAX_DOCUMENT_BYTES {
                return Err("the answer is larger than 256 KiB".to_owned());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| "the answer is not the expected JSON".to_owned())
    }
}

fn describe(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "the request timed out".to_owned()
    } else {
        "the authority could not be reached".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::entra::testing::{KEY_A, KEY_B, StubOpenId, TENANT};

    fn quick() -> KeyTiming {
        KeyTiming {
            max_age: Duration::from_secs(3600),
            stale_for: Duration::from_secs(3600),
            unknown_kid_cooldown: Duration::ZERO,
            retry_after_failure: Duration::ZERO,
            fetch_timeout: Duration::from_secs(2),
        }
    }

    fn tenant() -> Uuid {
        Uuid::parse_str(TENANT).unwrap()
    }

    #[tokio::test]
    async fn keys_are_fetched_once_and_cached() {
        let stub = StubOpenId::start().await;
        let keys = KeySets::new(&stub.settings(), quick()).unwrap();
        assert!(keys.key(tenant(), KEY_A.kid).await.unwrap().is_some());
        assert!(keys.key(tenant(), KEY_A.kid).await.unwrap().is_some());
        assert_eq!((stub.metadata_fetches(), stub.jwks_fetches()), (1, 1));
        // A tenant the settings do not list is never fetched.
        assert!(
            keys.key(Uuid::from_u128(7), KEY_A.kid)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(stub.metadata_fetches(), 1);
    }

    #[tokio::test]
    async fn a_keyword_shares_one_key_source_whatever_the_tenant() {
        let stub = StubOpenId::start().await;
        let settings = EntraSettings {
            tenants: Tenants::Organizations,
            ..stub.settings()
        };
        let keys = KeySets::new(&settings, quick()).unwrap();
        for n in 1..=5u128 {
            let tenant = Uuid::from_u128(0x70000000_0000_4000_8000_000000000000 + n);
            assert!(keys.key(tenant, KEY_A.kid).await.unwrap().is_some());
        }
        assert_eq!(
            (stub.metadata_fetches(), stub.jwks_fetches()),
            (1, 1),
            "one shared source, whose template issuer is not compared"
        );
        // Personal accounts are not organizations: nothing is fetched for them.
        assert!(
            keys.key(crate::entra::CONSUMERS_TENANT, KEY_A.kid)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(stub.metadata_fetches(), 1);
    }

    #[tokio::test]
    async fn an_unknown_kid_fetches_again_but_not_within_the_cooldown() {
        let stub = StubOpenId::start().await;
        let keys = KeySets::new(&stub.settings(), quick()).unwrap();
        keys.key(tenant(), KEY_A.kid).await.unwrap();
        stub.publish(&[KEY_A, KEY_B]);
        assert!(
            keys.key(tenant(), KEY_B.kid).await.unwrap().is_some(),
            "the rotated key is found without a restart"
        );
        assert_eq!(stub.jwks_fetches(), 2);

        let patient = KeySets::new(
            &stub.settings(),
            KeyTiming {
                unknown_kid_cooldown: Duration::from_secs(3600),
                ..quick()
            },
        )
        .unwrap();
        patient.key(tenant(), KEY_A.kid).await.unwrap();
        for kid in ["forged-1", "forged-2", "forged-3"] {
            assert!(patient.key(tenant(), kid).await.unwrap().is_none());
        }
        assert_eq!(
            stub.jwks_fetches(),
            3,
            "unknown kids within the cooldown fetch nothing"
        );
    }

    #[tokio::test]
    async fn an_outage_before_the_first_fetch_fails_closed_and_recovers() {
        let stub = StubOpenId::start().await;
        let keys = KeySets::new(&stub.settings(), quick()).unwrap();
        stub.set_failing(true);
        let error = keys.key(tenant(), KEY_A.kid).await.err().expect("no keys");
        assert!(
            matches!(&error, EntraError::Unavailable(reason) if reason.contains("HTTP 503")),
            "{error}"
        );
        stub.set_failing(false);
        assert!(keys.key(tenant(), KEY_A.kid).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn cached_keys_outlive_an_outage_until_they_are_too_old() {
        let stub = StubOpenId::start().await;
        let lenient = KeySets::new(
            &stub.settings(),
            KeyTiming {
                max_age: Duration::ZERO,
                ..quick()
            },
        )
        .unwrap();
        let strict = KeySets::new(
            &stub.settings(),
            KeyTiming {
                max_age: Duration::ZERO,
                stale_for: Duration::ZERO,
                ..quick()
            },
        )
        .unwrap();
        lenient.key(tenant(), KEY_A.kid).await.unwrap();
        strict.key(tenant(), KEY_A.kid).await.unwrap();
        stub.set_failing(true);
        assert!(
            lenient.key(tenant(), KEY_A.kid).await.unwrap().is_some(),
            "stale keys while the authority is down"
        );
        assert!(matches!(
            strict.key(tenant(), KEY_A.kid).await,
            Err(EntraError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_wrong_issuer_a_foreign_key_set_or_a_slow_authority_give_no_keys() {
        let stub = StubOpenId::start().await;
        stub.set_wrong_issuer(true);
        let error = KeySets::new(&stub.settings(), quick())
            .unwrap()
            .key(tenant(), KEY_A.kid)
            .await
            .err()
            .expect("no keys");
        assert!(error.to_string().contains("another issuer"), "{error}");
        stub.set_wrong_issuer(false);

        stub.set_foreign_jwks(true);
        let error = KeySets::new(&stub.settings(), quick())
            .unwrap()
            .key(tenant(), KEY_A.kid)
            .await
            .err()
            .expect("no keys");
        assert!(error.to_string().contains("authority host"), "{error}");
        assert_eq!(stub.jwks_fetches(), 0);
        stub.set_foreign_jwks(false);

        stub.set_delay(Duration::from_secs(5));
        let slow = KeySets::new(
            &stub.settings(),
            KeyTiming {
                fetch_timeout: Duration::from_millis(300),
                ..quick()
            },
        )
        .unwrap();
        let started = Instant::now();
        let error = slow.key(tenant(), KEY_A.kid).await.err().expect("no keys");
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
