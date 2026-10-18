//! A stub of Microsoft Entra ID for tests (feature `testing`): the OpenID metadata and key
//! set of any tenant over HTTP on 127.0.0.1, two fixed RSA-2048 test keys and a token
//! minter. TEST ONLY: the private keys below are public, so nothing may ever trust them.

use std::future::IntoFuture;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{CONSUMERS_TENANT, EntraSettings, Tenants};

/// The tenant the stub's tokens come from by default.
pub const TENANT: &str = "33333333-0000-4000-8000-000000000003";
/// A second tenant the stub's settings allow.
pub const OTHER_TENANT: &str = "44444444-0000-4000-8000-000000000004";
/// The API's application id: the audience of the stub's tokens.
pub const API_CLIENT_ID: &str = "11111111-0000-4000-8000-000000000001";
/// The client app (a SPA) that requests them: their `azp`.
pub const CLIENT_APP: &str = "22222222-0000-4000-8000-000000000002";

/// An RSA-2048 test key: the private key as base64 PKCS#1 DER, and the public modulus
/// and exponent as a JWK publishes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestKey {
    pub kid: &'static str,
    der: &'static str,
    n: &'static str,
    e: &'static str,
}

pub const KEY_A: TestKey = TestKey {
    kid: "test-key-a",
    der: "MIIEowIBAAKCAQEAwdzXVqnWLXr9hp64LltVmiOkJymuLAfiwVV7HK4QbmkByq/m7BP8xYqw4ycFxcr7RfjUl8gtCt4XQdOgD1a2/Os8T/1gDvA5W/S+Oec4Q2+x4uIWVzAjzvKO6/+d+LbGTMghaPNZG7rD88t1kCwxl7hHgO/7o5QpDQD9YPk+Yk2P+EPq/GWMBpzwsxEJTSJ43D1AsC0gsd2F0IjiJCZbEvJLRh9aAc457dOsMRgDIDeleSQy3vz7i3yTFNvm+W8CSs8aAetjkylnG4XcNWViXiq0l44VLnlQ5vwW4ecci7FwN07qWgBFM2A1lq0Sd3Xi8O5kNVaArlDLJ6YnkP0GTQIDAQABAoIBACC11JfI3FycLf7y9PzNaGozrVT2UW8YfYhFlhpGvHsEuMqsV3hp/vKN7j7FrNGs6CzsSWK64e69fosa3Bz6crHGN0z/4Z9jpSoA57/5ZaLrS8aOg1p+wuv+PWNQpCT9IgR+mDSPKu/k2X9fSxzieBOzgk0dMQZuz2ZkTlx4cbBAPk/pWT58XR0T2u/k+LJCf1nSFINBtlw+EnTe1MULNzceBQathT7Q4oTRS67KgfM/t9em8zrL9MKPYmIVR4LB6apLKNF5GyT3ckzE7mVT7lySJOn3L95nEK6F+W8R0d/IkOe+adP7Tr9oAOOYOSLZuJq7gaTFe99Qy6zUaiz6CLkCgYEAxJCaqlfjymYhf6afBmYw1JKWAoBOnIX3GlLoSfrpVjB6x6Zv/PW6HMrD0VLm5m96jETImqbUtYZtMH4vmqs+EHQujZ4c8G+N54yk7zZ0NsDF14f5kNUkUnbT5AscAXV00HYF4DGF2R6xBq4orRDCQt9Bd5JedP19eBM+4SCtcqsCgYEA/HsRulOI8TzRvzYGq9O/XhZpMQGnwC3VsF2CvQUTzg5g0ryQe3WYpMKquKY1ijLaYcX1BErhxFFcgFOLP5JcLioLBASQ/l2QIAiTh39T/A0/DBm3wZGc3+9gu8YwQJmhOIrN7zOcTEFZAnfi+Qf+4KYdbYh1XlHwSUXCMAr/qucCgYEAigcQ1HULG+FDvKOAH7o6p8qXVAJNMVRRBEcbnnd2ynMYhIG3OeZDXlxzzNVuFnpJj53tT8u+qglPOzIgs3d0CZHL7t4MbDmpZ+M3HlMaoCgVxRdShAVnkBCpEgVLESstcASwc5HS73fBz3MUVRLjlpm3GwPJGFW4eUzqZqPPqCkCgYBNUseYH9M1xmGaRMqAPyZBg22N06rzYVYrYEgU6YQqPB8NCzDYjqhD9VZhnI2TGIXnIcArI1/HRv+kUXG483MFtFyRS3C4ZiEonlfITIeFDu17IDk1sAmyT1Z5y3WloydL8L6m/lBkzjKbXeoGSJuXY2hpzjvbug5z8khJfGTWiwKBgCEhdl546X2oRQtgvRZVeJoy/Gdcdjk6lOje+VehJahx5HconR91TkxP82QGFfxqzvE+aGnc+rYluVOYPOxrTlmh+IZFMEby7GTECNp1KEYZ805ha95XW+dBYGNAn6RQo5rL6WYnTecDZuZ0/pts3pNPlW71T4CdUwKZAH3YBCrF",
    n: "wdzXVqnWLXr9hp64LltVmiOkJymuLAfiwVV7HK4QbmkByq_m7BP8xYqw4ycFxcr7RfjUl8gtCt4XQdOgD1a2_Os8T_1gDvA5W_S-Oec4Q2-x4uIWVzAjzvKO6_-d-LbGTMghaPNZG7rD88t1kCwxl7hHgO_7o5QpDQD9YPk-Yk2P-EPq_GWMBpzwsxEJTSJ43D1AsC0gsd2F0IjiJCZbEvJLRh9aAc457dOsMRgDIDeleSQy3vz7i3yTFNvm-W8CSs8aAetjkylnG4XcNWViXiq0l44VLnlQ5vwW4ecci7FwN07qWgBFM2A1lq0Sd3Xi8O5kNVaArlDLJ6YnkP0GTQ",
    e: "AQAB",
};

pub const KEY_B: TestKey = TestKey {
    kid: "test-key-b",
    der: "MIIEowIBAAKCAQEAv6lpHKf48AAhfMTU2uCwB+t3gXryGfX0SbcAIFypvmLcWCUg1VNFTkJkQP44DUnp3vik/8XvgTLNce//HTPDG3LJkU64ArTk/Z91dxA9n/rh0opMSXGZ67Gc9J98Zo3LZViiDjNbUGyRwXxD/ybpY4Sdl7mcr26d16aB9rHVULo42ttRCKb4Wwscnh2bXg363stt0nYH8SyBahXCo7z6hFDKgpoBj7cY5oiS9R+r4pHgRW2UhdIijAVXwS+89JUY6bjL/tq2Y7e9hE759GagH8+Yw1Zu9LscjjhYOwJI83TcjsvFAb8Z3lHK/nXkgmh23fdF1qk7e+AMrHus0Az3cQIDAQABAoIBAAfmJoPc1GCn0LmArzkaG4coCDPY6lNxlJDZLvMmfoRvNJ+ZYCIKnYedKt6W256Ilk/vNXWpJbbQbjEs7/m3IWpnL7GCagkKhYmCYef3TmLihx4TPxOjz/Erz8kxLYJYg7J4la06JqBI6TTtNywz4ViNpaUULEX6rBywKxkMuTrrb4LIlsPTTq388GqGDPvLQy3479oYlv68gh9ioY+blHAD3r0qIKSLrq/Cvr6TvvXGmmHuNq56bS9ksGzZ1gNCBR0UQsOuCty4Md3G3p9GnqehKxbMzPqWxVXPMLuc2ccw9rQ/naMFmA+BhGlakh77olwFICPGGKyrainXcLXBYOUCgYEA9eJDw3gvz2PAcHJ8tBcXzx+LsYq1vkJnUW/v4CbnKEkEsiCnnhyH3TYs0K626ioR928wij2YXY1llabnZ68/55RZhD7kCaWsEgC+KTGYozaLqkuQLf4e3qLZIdstpssB8OB/kbV9JWojSv9w17OUG7pnsQyOaHnmMAWidDOKH7MCgYEAx4wPTXQ22fyKa8VpaMLQvEOM5x4Z2JRfsRPDCNnNAJ4HTvjxjcCu+WVYxdibmj4uMcREwQvvfn7fd6Y/V4lhqx2zdaD9KceEpeyzj9K0Z2uCawLr5vr+cNsdYkaonlzi9c1a5vCVcrDhc6/hXivU5Gmm4cKYRSUJO2g8l8BsmksCgYANwwAv7IYggWC/6mh4W+KFQSq8YRsS7vyBXlZ6oTLG2m9c2N8IxmgERYf0lhK+wGQJCTwaQKBKfCN3YMQK1ZAqEe+Aq1RwBcm/qDL8cz4rxF3Exj3nlRV9OHgWoQHJGAufw7DgItxCP9KB3BIgwHhFpQUwr5c42bm8blRNVKUiaQKBgCB50PvAEDVF/pkvlcow+pvJOqNaqMYwz6oYF0PXfp+9GF9n8JgrZc69CceZSym9OPNsh+wLLz0dFO16JxDuEPEyckSQoOMtz47ZiRe9KRMz0hPsc8wxyTtAzfVynnq3j1CFzWeMfCYgV3DmjP0wkQNjln9jljiaoHgErOfdr08lAoGBAIC3LNKSQHkcxrFvkq99p+GI3rN/VBvny56j5DUEHcb/2rXTIZaZDQz7b6s+ZYf1EXLGYEBbFyDdVAiLrjLVy4c/i3qcb9xZlapdnorm5u6TjzSHwip04wjHpNhDmumeKXGXvb8VBoXGhQAO2G1C+jhWHTrIX/RH64KYTBFdUWsW",
    n: "v6lpHKf48AAhfMTU2uCwB-t3gXryGfX0SbcAIFypvmLcWCUg1VNFTkJkQP44DUnp3vik_8XvgTLNce__HTPDG3LJkU64ArTk_Z91dxA9n_rh0opMSXGZ67Gc9J98Zo3LZViiDjNbUGyRwXxD_ybpY4Sdl7mcr26d16aB9rHVULo42ttRCKb4Wwscnh2bXg363stt0nYH8SyBahXCo7z6hFDKgpoBj7cY5oiS9R-r4pHgRW2UhdIijAVXwS-89JUY6bjL_tq2Y7e9hE759GagH8-Yw1Zu9LscjjhYOwJI83TcjsvFAb8Z3lHK_nXkgmh23fdF1qk7e-AMrHus0Az3cQ",
    e: "AQAB",
};

impl TestKey {
    fn encoding(self) -> EncodingKey {
        EncodingKey::from_rsa_der(&STANDARD.decode(self.der).expect("the test key is base64"))
    }

    /// The public key as a JWKS entry.
    pub fn jwk(self) -> Value {
        json!({"kty": "RSA", "use": "sig", "kid": self.kid, "n": self.n, "e": self.e})
    }
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs() as i64
}

/// `claims` signed with `key` (RS256), naming `key`'s id.
pub fn sign(key: TestKey, claims: &Value) -> String {
    sign_as(key, key.kid, claims)
}

/// `claims` signed with `key` but naming the key `kid`: with another key's id, a token
/// whose signature does not match.
pub fn sign_as(key: TestKey, kid: &str, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_owned());
    jsonwebtoken::encode(&header, claims, &key.encoding()).expect("signing with a test key")
}

/// A token whose header names `alg` and `kid` and whose last part is `signature`: for
/// the algorithms the service must refuse (`none`, `HS256`, `RS384`, …).
pub fn with_algorithm(alg: &str, kid: &str, claims: &Value, signature: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(json!({"alg": alg, "kid": kid, "typ": "JWT"}).to_string());
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("{header}.{payload}.{signature}")
}

struct Stub {
    authority: String,
    published: Mutex<Vec<TestKey>>,
    failing: AtomicBool,
    delay_ms: AtomicU64,
    foreign_jwks: AtomicBool,
    wrong_issuer: AtomicBool,
    metadata_fetches: AtomicUsize,
    jwks_fetches: AtomicUsize,
}

/// The stub authority. It publishes [`KEY_A`] until told otherwise.
pub struct StubOpenId {
    state: Arc<Stub>,
}

impl StubOpenId {
    /// The stub on 127.0.0.1 over plain HTTP.
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the stub issuer");
        let authority = format!("http://{}/", listener.local_addr().expect("its address"));
        let (stub, router) = Self::with_authority(&authority);
        tokio::spawn(axum::serve(listener, router).into_future());
        stub
    }

    /// The stub and its routes, for a server of the caller's (an HTTPS one, for example)
    /// reachable at `authority` (scheme and host, ending in `/`).
    pub fn with_authority(authority: &str) -> (Self, Router) {
        let state = Arc::new(Stub {
            authority: authority.to_owned(),
            published: Mutex::new(vec![KEY_A]),
            failing: AtomicBool::new(false),
            delay_ms: AtomicU64::new(0),
            foreign_jwks: AtomicBool::new(false),
            wrong_issuer: AtomicBool::new(false),
            metadata_fetches: AtomicUsize::new(0),
            jwks_fetches: AtomicUsize::new(0),
        });
        let router = Router::new()
            .route(
                "/{tenant}/v2.0/.well-known/openid-configuration",
                get(metadata),
            )
            .route("/{tenant}/discovery/v2.0/keys", get(keys))
            .with_state(Arc::clone(&state));
        (Self { state }, router)
    }

    /// The value for `PUSH_ENTRA_AUTHORITY_HOST`, ending in `/`.
    pub fn authority(&self) -> &str {
        &self.state.authority
    }

    /// The `iss` of `tenant`'s tokens.
    pub fn issuer(&self, tenant: &str) -> String {
        format!("{}{tenant}/v2.0", self.state.authority)
    }

    /// Publishes exactly `keys` from now on: a rotation.
    pub fn publish(&self, keys: &[TestKey]) {
        *self.state.published.lock().unwrap() = keys.to_vec();
    }

    /// While set, every request answers 503.
    pub fn set_failing(&self, failing: bool) {
        self.state.failing.store(failing, Ordering::SeqCst);
    }

    /// Every answer waits this long first.
    pub fn set_delay(&self, delay: Duration) {
        self.state
            .delay_ms
            .store(delay.as_millis() as u64, Ordering::SeqCst);
    }

    /// While set, the metadata names a key set on another host.
    pub fn set_foreign_jwks(&self, foreign: bool) {
        self.state.foreign_jwks.store(foreign, Ordering::SeqCst);
    }

    /// While set, the metadata names another issuer.
    pub fn set_wrong_issuer(&self, wrong: bool) {
        self.state.wrong_issuer.store(wrong, Ordering::SeqCst);
    }

    /// Metadata documents served so far (failed ones included).
    pub fn metadata_fetches(&self) -> usize {
        self.state.metadata_fetches.load(Ordering::SeqCst)
    }

    /// Key sets served so far (failed ones included).
    pub fn jwks_fetches(&self) -> usize {
        self.state.jwks_fetches.load(Ordering::SeqCst)
    }

    /// Settings that trust this stub: [`TENANT`] and [`OTHER_TENANT`], [`API_CLIENT_ID`],
    /// `access_as_user`, 24 hours.
    pub fn settings(&self) -> EntraSettings {
        EntraSettings {
            authority_host: self.state.authority.clone(),
            tenants: Tenants::Ids(vec![
                Uuid::parse_str(TENANT).expect("a GUID"),
                Uuid::parse_str(OTHER_TENANT).expect("a GUID"),
            ]),
            audiences: vec![API_CLIENT_ID.to_owned()],
            required_scope: Some("access_as_user".to_owned()),
            required_role: None,
            client_apps: Vec::new(),
            max_session: Duration::from_secs(24 * 3600),
        }
    }

    /// auth-svc's variables for Entra-only sign-in trusting this stub.
    pub fn variables(&self) -> Vec<(String, String)> {
        vec![
            ("PUSH_AUTH_PROVIDERS".to_owned(), "entra".to_owned()),
            (
                "PUSH_ENTRA_AUTHORITY_HOST".to_owned(),
                self.state.authority.clone(),
            ),
            (
                "PUSH_ENTRA_ALLOWED_TENANTS".to_owned(),
                format!("{TENANT},{OTHER_TENANT}"),
            ),
            ("PUSH_ENTRA_CLIENT_ID".to_owned(), API_CLIENT_ID.to_owned()),
        ]
    }

    /// The claims of a delegated v2.0 access token for [`API_CLIENT_ID`] from `tenant`,
    /// for the user `object_id`, valid for an hour.
    pub fn user_claims(&self, tenant: &str, object_id: &str) -> Value {
        let now = unix_now();
        json!({
            "aud": API_CLIENT_ID,
            "iss": self.issuer(tenant),
            "iat": now,
            "nbf": now,
            "exp": now + 3600,
            "ver": "2.0",
            "tid": tenant,
            "oid": object_id,
            "sub": format!("pairwise-{object_id}"),
            "scp": "access_as_user",
            "azp": CLIENT_APP,
            "azpacr": "0",
            "name": "Test User",
            "preferred_username": "test.user@contoso.test",
        })
    }

    /// A valid token of [`TENANT`]'s user `object_id`, signed with [`KEY_A`].
    pub fn token(&self, object_id: &str) -> String {
        sign(KEY_A, &self.user_claims(TENANT, object_id))
    }
}

/// Waits the configured delay; `Some(503)` while failing.
async fn pause(stub: &Stub) -> Option<Response> {
    let delay = stub.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    stub.failing
        .load(Ordering::SeqCst)
        .then(|| StatusCode::SERVICE_UNAVAILABLE.into_response())
}

async fn metadata(State(stub): State<Arc<Stub>>, Path(tenant): Path<String>) -> Response {
    stub.metadata_fetches.fetch_add(1, Ordering::SeqCst);
    if let Some(refused) = pause(&stub).await {
        return refused;
    }
    // As Entra ID answers: the shared endpoints of common and organizations name a
    // template issuer, consumers' names the personal-account tenant.
    let issuer = if stub.wrong_issuer.load(Ordering::SeqCst) {
        format!("https://sts.example.test/{tenant}/v2.0")
    } else {
        match tenant.as_str() {
            "common" | "organizations" => format!("{}{{tenantid}}/v2.0", stub.authority),
            "consumers" => format!("{}{CONSUMERS_TENANT}/v2.0", stub.authority),
            _ => format!("{}{tenant}/v2.0", stub.authority),
        }
    };
    let jwks_uri = if stub.foreign_jwks.load(Ordering::SeqCst) {
        "http://203.0.113.9/keys".to_owned()
    } else {
        format!("{}{tenant}/discovery/v2.0/keys", stub.authority)
    };
    Json(json!({
        "issuer": issuer,
        "jwks_uri": jwks_uri,
        "id_token_signing_alg_values_supported": ["RS256"],
    }))
    .into_response()
}

async fn keys(State(stub): State<Arc<Stub>>) -> Response {
    stub.jwks_fetches.fetch_add(1, Ordering::SeqCst);
    if let Some(refused) = pause(&stub).await {
        return refused;
    }
    let keys: Vec<Value> = stub
        .published
        .lock()
        .unwrap()
        .iter()
        .map(|key| key.jwk())
        .collect();
    Json(json!({ "keys": keys })).into_response()
}

#[cfg(test)]
mod tests {
    use jsonwebtoken::{DecodingKey, Validation};

    use super::*;

    #[test]
    fn each_test_key_verifies_its_own_signatures_only() {
        let claims = json!({"aud": API_CLIENT_ID, "exp": unix_now() + 60});
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[API_CLIENT_ID]);
        for (key, other) in [(KEY_A, KEY_B), (KEY_B, KEY_A)] {
            let decoding = DecodingKey::from_rsa_components(key.n, key.e).unwrap();
            let own = jsonwebtoken::decode::<Value>(&sign(key, &claims), &decoding, &validation);
            assert!(own.is_ok(), "{own:?}");
            let foreign =
                jsonwebtoken::decode::<Value>(&sign(other, &claims), &decoding, &validation);
            assert!(foreign.is_err());
        }
    }

    #[tokio::test]
    async fn the_stub_publishes_metadata_and_keys_for_any_tenant() {
        let stub = StubOpenId::start().await;
        let base = stub.authority().to_owned();
        let metadata: Value = reqwest::get(format!(
            "{base}{TENANT}/v2.0/.well-known/openid-configuration"
        ))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        assert_eq!(metadata["issuer"], stub.issuer(TENANT));
        let keys: Value = reqwest::get(metadata["jwks_uri"].as_str().unwrap())
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(keys["keys"][0]["kid"], KEY_A.kid);
        stub.set_failing(true);
        let refused = reqwest::get(format!("{base}{TENANT}/discovery/v2.0/keys"))
            .await
            .unwrap();
        assert_eq!(refused.status(), 503);
        assert_eq!((stub.metadata_fetches(), stub.jwks_fetches()), (1, 2));
    }
}
