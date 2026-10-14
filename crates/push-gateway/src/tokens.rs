//! Checking access tokens without calling auth-svc (spec §5.1, §7.3). auth-svc signs them
//! with Ed25519 (EdDSA) and publishes its public keys as a JSON Web Key Set at
//! `GET /v1/auth/jwks`. The gateway keeps those keys by `kid` and fetches the set again
//! when a token names a key it does not know, so auth-svc can rotate its key without the
//! gateway restarting.
//!
//! A rotation signs nobody out. auth-svc publishes only its current key, so a key that
//! leaves the set is kept for [`RETIRED_KEY_TTL`] (an access token's 15 minutes plus the
//! leeway): tokens it signed keep passing until they expire. A token naming a key the
//! gateway does not hold even after fetching the set for it, such as a key retired before
//! this gateway started, is `Expired` (refresh, then reconnect), never `Invalid`: a
//! refresh gets a token signed with the current key. Retiring a compromised key at once
//! therefore takes a restart of the gateways.
//!
//! A token passes only if it names a known key with `kid`, is signed with EdDSA (the
//! algorithm is pinned: the header's `alg` cannot choose another), names this gateway in
//! `aud`, carries `exp` (required: jsonwebtoken 9 skips a mistyped claim unless it is
//! required, CVE-2026-25537), `sub`, `device_id` and `iat`, says the email is verified,
//! and has not expired more than [`LEEWAY`] ago.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::time::Instant;
use uuid::Uuid;

/// How far past `exp` a token is still accepted: clock skew between auth-svc and the
/// gateway.
pub const LEEWAY: Duration = Duration::from_secs(60);
/// A token naming an unknown key fetches the key set again at most this often.
pub const REFETCH_INTERVAL: Duration = Duration::from_secs(30);
/// How long a key that left the set keeps verifying: the 15 minutes an access token lives,
/// plus the leeway.
pub const RETIRED_KEY_TTL: Duration = Duration::from_secs(15 * 60 + LEEWAY.as_secs());
/// The largest key set read; auth-svc's is under 1 KiB.
pub const MAX_KEY_SET_BYTES: usize = 64 * 1024;
/// How long fetching the key set may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// What a valid access token says: user `user` on device `device`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claims {
    pub user: Uuid,
    pub device: Uuid,
    /// `iat` and `exp`, in seconds since the Unix epoch.
    pub issued_at: i64,
    pub expires_at: i64,
}

/// Why a token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// Not a token of ours for this gateway: sign in again (close code 4001).
    #[error("the access token is not valid")]
    Invalid,
    /// Ours, but expired: refresh it (close code 4002).
    #[error("the access token has expired")]
    Expired,
    /// The key set could not be fetched, so the token could not be checked: retry later
    /// (close code 1011). Never a reason to sign out.
    #[error("the token keys are unavailable")]
    KeysUnavailable,
}

/// The claims auth-svc puts in an access token; `aud` and `exp` are checked by
/// jsonwebtoken. A missing or mistyped field fails the decoding.
#[derive(Deserialize)]
struct AccessClaims {
    sub: Uuid,
    device_id: Uuid,
    email_verified: bool,
    exp: i64,
    iat: i64,
}

/// The access-token checker, shared by every connection.
#[derive(Clone)]
pub struct Verifier {
    inner: Arc<Inner>,
}

struct Inner {
    jwks_url: String,
    validation: Validation,
    http: reqwest::Client,
    keys: RwLock<HashMap<String, Key>>,
    /// Serialises fetches, and holds when a token naming an unknown key last fetched.
    refetched: tokio::sync::Mutex<Option<Instant>>,
    refetch_interval: Duration,
    retired_key_ttl: Duration,
}

/// A key held by `kid`.
#[derive(Clone)]
struct Key {
    key: DecodingKey,
    /// When the key was first missing from a fetched set; `None` while it is published.
    retired_at: Option<Instant>,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Verifier")
            .field("jwks_url", &self.inner.jwks_url)
            .field("keys", &self.key_ids())
            .finish()
    }
}

impl Verifier {
    /// A checker for tokens whose audience is `audience` (the gateway's public WebSocket
    /// URL), with keys from `jwks_url`. It has no keys until [`Self::fetch`] succeeds.
    pub fn new(jwks_url: impl Into<String>, audience: &str) -> Self {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(&[audience]);
        validation.set_required_spec_claims(&["exp", "sub", "aud"]);
        validation.leeway = LEEWAY.as_secs();
        validation.validate_nbf = false;
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("a client without TLS or proxies always builds");
        Self {
            inner: Arc::new(Inner {
                jwks_url: jwks_url.into(),
                validation,
                http,
                keys: RwLock::new(HashMap::new()),
                refetched: tokio::sync::Mutex::new(None),
                refetch_interval: REFETCH_INTERVAL,
                retired_key_ttl: RETIRED_KEY_TTL,
            }),
        }
    }

    /// Whether any key is loaded, so tokens can be checked.
    pub fn is_ready(&self) -> bool {
        !self.read_keys().is_empty()
    }

    /// The ids of the keys held, published or retired, sorted.
    pub fn key_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.read_keys().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Fetches the key set and takes its Ed25519 keys; keys of other kinds, and keys
    /// without a `kid`, are skipped. A key that left the set is kept for
    /// [`RETIRED_KEY_TTL`] from the first fetch that missed it. A failed fetch, or a set
    /// over [`MAX_KEY_SET_BYTES`], changes nothing. The error never quotes the response.
    pub async fn fetch(&self) -> Result<(), String> {
        let _serialised = self.inner.refetched.lock().await;
        self.load().await
    }

    async fn load(&self) -> Result<(), String> {
        let response = self
            .inner
            .http
            .get(&self.inner.jwks_url)
            .send()
            .await
            .map_err(|error| format!("cannot reach the key set: {}", kind(&error)))?;
        if !response.status().is_success() {
            return Err(format!("the key set answered {}", response.status()));
        }
        let body = read_bounded(response, MAX_KEY_SET_BYTES).await?;
        let set: KeySet = serde_json::from_slice(&body)
            .map_err(|_| "the key set is not a JSON Web Key Set".to_owned())?;
        let published = decoding_keys(set);
        if published.is_empty() {
            return Err("the key set has no Ed25519 key with a kid".to_owned());
        }
        let now = Instant::now();
        let mut keys = self.inner.keys.write().expect("keys lock poisoned");
        keys.retain(|kid, held| {
            if published.contains_key(kid) {
                return true;
            }
            let retired_at = *held.retired_at.get_or_insert(now);
            now.duration_since(retired_at) < self.inner.retired_key_ttl
        });
        for (kid, key) in published {
            keys.insert(
                kid,
                Key {
                    key,
                    retired_at: None,
                },
            );
        }
        Ok(())
    }

    /// The claims of `token`, if it passes every check.
    pub async fn verify(&self, token: &str) -> Result<Claims, TokenError> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| TokenError::Invalid)?;
        if header.alg != Algorithm::EdDSA {
            return Err(TokenError::Invalid);
        }
        let kid = header.kid.ok_or(TokenError::Invalid)?;
        let key = match self.key(&kid) {
            Some(key) => key,
            None => self.refetch_for(&kid).await?,
        };
        let data = jsonwebtoken::decode::<AccessClaims>(token, &key, &self.inner.validation)
            .map_err(|error| match error.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => TokenError::Expired,
                _ => TokenError::Invalid,
            })?;
        let claims = data.claims;
        if !claims.email_verified {
            return Err(TokenError::Invalid);
        }
        Ok(Claims {
            user: claims.sub,
            device: claims.device_id,
            issued_at: claims.iat,
            expires_at: claims.exp,
        })
    }

    fn key(&self, kid: &str) -> Option<DecodingKey> {
        self.read_keys().get(kid).map(|held| held.key.clone())
    }

    /// A token names a key the gateway does not have: auth-svc may have rotated its key.
    /// Fetches the set again, at most every [`REFETCH_INTERVAL`] whatever the key ids
    /// tokens name, so made-up ones cannot make the gateway hammer auth-svc. A key still
    /// missing from a set fetched for this very token is `Expired`: it may be a key
    /// retired before this gateway learnt of it, and a refresh gets a token signed with
    /// the current key, while a made-up key gets nobody in. A key that could not be looked
    /// up, because the fetch failed or came too soon after the last one, is
    /// `KeysUnavailable` (try again). Neither an auth-svc outage nor a rotation signs
    /// anyone out.
    async fn refetch_for(&self, kid: &str) -> Result<DecodingKey, TokenError> {
        let mut refetched = self.inner.refetched.lock().await;
        // Another connection may have fetched while this one waited.
        if let Some(key) = self.key(kid) {
            return Ok(key);
        }
        if refetched.is_some_and(|at| at.elapsed() < self.inner.refetch_interval) {
            return Err(TokenError::KeysUnavailable);
        }
        *refetched = Some(Instant::now());
        if let Err(error) = self.load().await {
            tracing::warn!(%error, "could not fetch the token keys");
            return Err(TokenError::KeysUnavailable);
        }
        self.key(kid).ok_or(TokenError::Expired)
    }

    fn read_keys(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Key>> {
        self.inner.keys.read().expect("keys lock poisoned")
    }
}

/// The body of `response`, refused once it exceeds `limit` bytes, whatever its
/// `content-length` says.
async fn read_bounded(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    let too_large = || format!("the key set exceeds {limit} bytes");
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("cannot read the key set: {}", kind(&error)))?
    {
        if body.len() + chunk.len() > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A key set read loosely: one key auth-svc adds in a form this gateway does not know
/// must not hide the others.
#[derive(Deserialize)]
struct KeySet {
    keys: Vec<serde_json::Value>,
}

/// The Ed25519 keys of a key set, by `kid`.
fn decoding_keys(set: KeySet) -> HashMap<String, DecodingKey> {
    set.keys
        .into_iter()
        .filter_map(|value| serde_json::from_value::<Jwk>(value).ok())
        .filter_map(|jwk| {
            let AlgorithmParameters::OctetKeyPair(pair) = &jwk.algorithm else {
                return None;
            };
            if pair.curve != EllipticCurve::Ed25519 {
                return None;
            }
            let kid = jwk.common.key_id.clone()?;
            let key = DecodingKey::from_jwk(&jwk).ok()?;
            Some((kid, key))
        })
        .collect()
}

/// What kind of failure a request met, without its URL.
fn kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "connection refused or unreachable"
    } else {
        "request failed"
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::routing::get;
    use jsonwebtoken::{EncodingKey, Header};
    use push_auth::tokens::AccessTokens;
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    const AUDIENCE: &str = "wss://push.example.com/v1/socket";
    const SEED: [u8; 32] = [7; 32];

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn issuer(seed: [u8; 32]) -> AccessTokens {
        AccessTokens::from_seed(&seed, AUDIENCE).unwrap()
    }

    /// Serves `jwks` at `/v1/auth/jwks`, answering the latest value it was given, and
    /// counts the requests.
    async fn key_server(
        jwks: serde_json::Value,
    ) -> (
        String,
        Arc<std::sync::Mutex<serde_json::Value>>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let current = Arc::new(std::sync::Mutex::new(jwks));
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (served, counted) = (Arc::clone(&current), Arc::clone(&hits));
        let app = Router::new().route(
            "/v1/auth/jwks",
            get(move || {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = served.lock().unwrap().clone();
                async move { axum::Json(body) }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/auth/jwks", listener.local_addr().unwrap());
        tokio::spawn(axum::serve(listener, app).into_future());
        (url, current, hits)
    }

    async fn verifier_for(issuer: &AccessTokens) -> Verifier {
        let (url, _, _) = key_server(issuer.jwks()).await;
        let verifier = Verifier::new(url, AUDIENCE);
        verifier.fetch().await.unwrap();
        verifier
    }

    /// A token with these claims and header, signed with the seed's key.
    fn signed(seed: [u8; 32], header: Header, claims: serde_json::Value) -> String {
        // PKCS#8 v1 of an Ed25519 seed, as auth-svc builds it.
        let mut der = vec![
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        der.extend_from_slice(&seed);
        jsonwebtoken::encode(&header, &claims, &EncodingKey::from_ed_der(&der)).unwrap()
    }

    fn eddsa(kid: &str) -> Header {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(kid.to_owned());
        header
    }

    fn claims(exp: i64) -> serde_json::Value {
        json!({
            "sub": Uuid::now_v7(),
            "device_id": Uuid::now_v7(),
            "email_verified": true,
            "exp": exp,
            "iat": exp - 900,
            "jti": Uuid::now_v7(),
            "aud": AUDIENCE,
        })
    }

    #[tokio::test]
    async fn a_token_from_auth_svc_passes_with_its_claims() {
        let issuer = issuer(SEED);
        let verifier = verifier_for(&issuer).await;
        assert!(verifier.is_ready());
        assert_eq!(verifier.key_ids(), [issuer.kid()]);
        let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
        let (token, issued) = issuer.issue(user, device, now());
        assert_eq!(
            verifier.verify(&token).await,
            Ok(Claims {
                user,
                device,
                issued_at: issued.iat,
                expires_at: issued.exp,
            })
        );
    }

    #[tokio::test]
    async fn an_expired_token_is_expired_only_after_the_leeway() {
        let issuer = issuer(SEED);
        let verifier = verifier_for(&issuer).await;
        let user = Uuid::now_v7();
        // Expired 30 s ago: within the 60 s leeway.
        let (fresh_enough, _) = issuer.issue(user, user, now() - 900 - 30);
        assert!(verifier.verify(&fresh_enough).await.is_ok());
        let (expired, _) = issuer.issue(user, user, now() - 900 - 90);
        assert_eq!(verifier.verify(&expired).await, Err(TokenError::Expired));
    }

    #[tokio::test]
    async fn tokens_for_another_audience_or_from_another_key_are_invalid() {
        let issuer = issuer(SEED);
        let verifier = verifier_for(&issuer).await;
        let user = Uuid::now_v7();
        let elsewhere = AccessTokens::from_seed(&SEED, "wss://other.example.com/v1/socket");
        let (other_audience, _) = elsewhere.unwrap().issue(user, user, now());
        assert_eq!(
            verifier.verify(&other_audience).await,
            Err(TokenError::Invalid)
        );
        // Signed by another key that claims our kid.
        let forged = signed([8; 32], eddsa(issuer.kid()), claims(now() + 600));
        assert_eq!(verifier.verify(&forged).await, Err(TokenError::Invalid));
        assert_eq!(
            verifier.verify("not.a.token").await,
            Err(TokenError::Invalid)
        );
    }

    #[tokio::test]
    async fn exp_kid_eddsa_and_a_verified_email_are_required() {
        let issuer = issuer(SEED);
        let verifier = verifier_for(&issuer).await;
        let kid = issuer.kid();
        let mut without_exp = claims(now() + 600);
        without_exp.as_object_mut().unwrap().remove("exp");
        let mut mistyped_exp = claims(now() + 600);
        mistyped_exp["exp"] = json!("tomorrow");
        let mut without_device = claims(now() + 600);
        without_device.as_object_mut().unwrap().remove("device_id");
        let mut unverified = claims(now() + 600);
        unverified["email_verified"] = json!(false);
        for claims in [without_exp, mistyped_exp, without_device, unverified] {
            let token = signed(SEED, eddsa(kid), claims);
            assert_eq!(verifier.verify(&token).await, Err(TokenError::Invalid));
        }
        let mut no_kid = eddsa(kid);
        no_kid.kid = None;
        let token = signed(SEED, no_kid, claims(now() + 600));
        assert_eq!(verifier.verify(&token).await, Err(TokenError::Invalid));
        // The header cannot choose another algorithm, such as HMAC keyed with the public
        // key.
        let mut hmac = Header::new(Algorithm::HS256);
        hmac.kid = Some(kid.to_owned());
        let token = jsonwebtoken::encode(
            &hmac,
            &claims(now() + 600),
            &EncodingKey::from_secret(b"anything"),
        )
        .unwrap();
        assert_eq!(verifier.verify(&token).await, Err(TokenError::Invalid));
    }

    #[tokio::test]
    async fn a_rotated_key_is_fetched_when_a_token_names_it() {
        let (old, new) = (issuer(SEED), issuer([9; 32]));
        let (url, jwks, hits) = key_server(old.jwks()).await;
        let mut verifier = Verifier::new(url, AUDIENCE);
        verifier.fetch().await.unwrap();
        // auth-svc rotates: the set now holds both keys.
        let both = json!({
            "keys": [old.jwks()["keys"][0].clone(), new.jwks()["keys"][0].clone()]
        });
        *jwks.lock().unwrap() = both;
        let user = Uuid::now_v7();
        let (token, _) = new.issue(user, user, now());
        assert!(verifier.verify(&token).await.is_ok());
        let (token, _) = old.issue(user, user, now());
        assert!(verifier.verify(&token).await.is_ok());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);

        // Made-up kids fetch at most every 30 s; until then they cannot be checked.
        let stranger = signed([10; 32], eddsa("unknown-kid"), claims(now() + 600));
        for _ in 0..3 {
            assert_eq!(
                verifier.verify(&stranger).await,
                Err(TokenError::KeysUnavailable)
            );
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        // A key missing from a set fetched for the token itself gets nobody in, and signs
        // nobody out: refresh.
        Arc::get_mut(&mut verifier.inner).unwrap().refetch_interval = Duration::ZERO;
        assert_eq!(verifier.verify(&stranger).await, Err(TokenError::Expired));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_rotation_signs_nobody_out() {
        let (k1, k2) = (issuer(SEED), issuer([9; 32]));
        let (url, jwks, _) = key_server(k1.jwks()).await;
        let mut verifier = Verifier::new(url.clone(), AUDIENCE);
        verifier.fetch().await.unwrap();
        let user = Uuid::now_v7();
        let (old_token, _) = k1.issue(user, user, now());
        // auth-svc rotates and publishes only the new key; the 5-minute fetch sees it.
        *jwks.lock().unwrap() = k2.jwks();
        verifier.fetch().await.unwrap();
        let mut held = vec![k1.kid().to_owned(), k2.kid().to_owned()];
        held.sort();
        assert_eq!(verifier.key_ids(), held);
        assert!(verifier.verify(&old_token).await.is_ok());
        let (new_token, _) = k2.issue(user, user, now());
        assert!(verifier.verify(&new_token).await.is_ok());

        // Once the old key's tokens have all expired it is dropped; a token naming it
        // asks for a refresh, not a sign-in.
        let inner = Arc::get_mut(&mut verifier.inner).unwrap();
        inner.retired_key_ttl = Duration::ZERO;
        inner.refetch_interval = Duration::ZERO;
        verifier.fetch().await.unwrap();
        assert_eq!(verifier.key_ids(), [k2.kid()]);
        assert_eq!(verifier.verify(&old_token).await, Err(TokenError::Expired));

        // So does a gateway started after the rotation, which never held the old key.
        let late = Verifier::new(url, AUDIENCE);
        late.fetch().await.unwrap();
        assert_eq!(late.verify(&old_token).await, Err(TokenError::Expired));
    }

    #[tokio::test]
    async fn an_oversized_key_set_is_refused_and_changes_nothing() {
        let issuer = issuer(SEED);
        let (url, jwks, _) = key_server(issuer.jwks()).await;
        let verifier = Verifier::new(url, AUDIENCE);
        verifier.fetch().await.unwrap();
        let padding = "x".repeat(MAX_KEY_SET_BYTES);
        *jwks.lock().unwrap() = json!({
            "keys": [issuer.jwks()["keys"][0].clone(), {"kty": "padding", "x": padding}]
        });
        let error = verifier.fetch().await.unwrap_err();
        assert!(error.contains("exceeds"), "{error}");
        assert_eq!(verifier.key_ids(), [issuer.kid()]);
        let (token, _) = issuer.issue(Uuid::now_v7(), Uuid::now_v7(), now());
        assert!(verifier.verify(&token).await.is_ok());
    }

    #[tokio::test]
    async fn without_reachable_keys_tokens_cannot_be_checked_and_nobody_is_signed_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/auth/jwks", listener.local_addr().unwrap());
        drop(listener);
        let verifier = Verifier::new(url, AUDIENCE);
        assert!(verifier.fetch().await.is_err());
        assert!(!verifier.is_ready());
        let issuer = issuer(SEED);
        let (token, _) = issuer.issue(Uuid::now_v7(), Uuid::now_v7(), now());
        assert_eq!(
            verifier.verify(&token).await,
            Err(TokenError::KeysUnavailable)
        );
    }

    #[test]
    fn keys_of_other_kinds_or_without_a_kid_are_skipped() {
        let issuer = issuer(SEED);
        let ours = issuer.jwks()["keys"][0].clone();
        let mut without_kid = ours.clone();
        without_kid.as_object_mut().unwrap().remove("kid");
        let set = KeySet {
            keys: vec![
                json!({"kty": "RSA", "kid": "rsa", "n": "AQAB", "e": "AQAB", "alg": "RS256"}),
                json!({"kty": "unknown"}),
                without_kid,
                ours,
            ],
        };
        let keys = decoding_keys(set);
        assert_eq!(keys.keys().collect::<Vec<_>>(), [issuer.kid()]);
    }
}
