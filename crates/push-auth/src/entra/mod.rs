//! Microsoft Entra ID sign-in (spec 2026-10-19): auth-svc checks an Entra ID access token
//! issued for its API and finds the user by the token's immutable `(tid, oid)`. It never
//! redeems authorization codes and holds no client secret. Tokens, object ids, names and
//! email addresses are never logged.

mod claims;
mod keys;
mod settings;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, Validation};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

pub use keys::KeyTiming;
pub use settings::{
    CONSUMERS_TENANT, DEFAULT_AUTHORITY_HOST, DEFAULT_REQUIRED_SCOPE, EntraSettings, Providers,
    Tenants,
};

/// Why a token was refused. Logged by name; the client only ever hears
/// `entra_token_invalid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Malformed,
    Algorithm,
    Tenant,
    Issuer,
    Signature,
    Audience,
    Expired,
    NotYetValid,
    Version,
    AppOnly,
    Scope,
    Role,
    ClientApp,
    Subject,
}

impl Reason {
    /// The name in the security log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::Algorithm => "algorithm",
            Self::Tenant => "tenant",
            Self::Issuer => "issuer",
            Self::Signature => "signature",
            Self::Audience => "audience",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::Version => "version",
            Self::AppOnly => "app_only",
            Self::Scope => "scope",
            Self::Role => "role",
            Self::ClientApp => "client_app",
            Self::Subject => "subject",
        }
    }
}

/// Why no identity came out of a token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntraError {
    #[error("the Entra ID token is refused ({})", .0.as_str())]
    Rejected(Reason),
    /// No usable signing keys: the authority could not be reached or answered nonsense.
    #[error("Microsoft Entra ID's signing keys are not available: {0}")]
    Unavailable(String),
}

/// The user an accepted token names. `Debug` hides everything but the tenant.
#[derive(Clone, PartialEq, Eq)]
pub struct EntraIdentity {
    pub tenant_id: Uuid,
    /// The user's object id: immutable, the same for every app of the tenant.
    pub object_id: Uuid,
    /// The `name` claim: a display-name suggestion for a new account only.
    pub name: Option<String>,
    /// `email`, else `preferred_username`: informational, never used to find an account.
    pub email: Option<String>,
}

impl std::fmt::Debug for EntraIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EntraIdentity")
            .field("tenant_id", &self.tenant_id)
            .field("object_id", &"<redacted>")
            .field("name", &self.name.as_ref().map(|_| "<redacted>"))
            .field("email", &self.email.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The longest token accepted, in bytes.
const MAX_TOKEN_BYTES: usize = 8 * 1024;
/// Clock skew allowed on `exp` and `nbf`.
pub const LEEWAY_SECONDS: u64 = 60;

/// Checks Entra ID access tokens for this API (spec D2): RS256 with a published key of
/// the key source of an admitted tenant, the v2.0 issuer of the token's own tenant, a configured audience, `exp`/`nbf`
/// within 60 s, then the claim rules.
#[derive(Clone)]
pub struct EntraSignIn {
    settings: Arc<EntraSettings>,
    keys: Arc<keys::KeySets>,
}

impl std::fmt::Debug for EntraSignIn {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EntraSignIn")
            .field("tenants", &self.settings.tenants)
            .field("audiences", &self.settings.audiences)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TokenHeader {
    #[serde(default)]
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

/// What is read before the signature is checked, only to choose the tenant and to
/// refuse tokens of other tenants without fetching anything.
#[derive(Deserialize)]
struct Unverified {
    #[serde(default)]
    tid: Option<String>,
    #[serde(default)]
    iss: Option<String>,
}

impl EntraSignIn {
    pub fn new(settings: EntraSettings) -> Result<Self, String> {
        Self::with_timing(settings, KeyTiming::default())
    }

    /// As [`EntraSignIn::new`] with other key timings (tests).
    pub fn with_timing(settings: EntraSettings, timing: KeyTiming) -> Result<Self, String> {
        let keys = keys::KeySets::new(&settings, timing)?;
        Ok(Self {
            settings: Arc::new(settings),
            keys: Arc::new(keys),
        })
    }

    pub fn settings(&self) -> &EntraSettings {
        &self.settings
    }

    /// How long an Entra-backed device stays signed in (spec D8).
    pub fn max_session(&self) -> Duration {
        self.settings.max_session
    }

    /// Fetches every allowed tenant's keys once; failures are logged.
    pub async fn warm(&self) {
        self.keys.warm().await;
    }

    /// The user `token` names, if it is an acceptable access token for this API. A
    /// refusal is logged with its reason, never with the token or its claims.
    pub async fn verify(&self, token: &str) -> Result<EntraIdentity, EntraError> {
        let checked = self.check(token).await;
        match &checked {
            Err(EntraError::Rejected(reason)) => tracing::warn!(
                target: "security",
                event = "auth.entra.rejected",
                reason = reason.as_str(),
                "an Entra ID access token was refused"
            ),
            Err(EntraError::Unavailable(error)) => {
                tracing::warn!(%error, "an Entra ID sign-in failed: no usable signing keys");
            }
            Ok(_) => {}
        }
        checked
    }

    async fn check(&self, token: &str) -> Result<EntraIdentity, EntraError> {
        let malformed = EntraError::Rejected(Reason::Malformed);
        let parts: Vec<&str> = token.split('.').collect();
        if token.len() > MAX_TOKEN_BYTES || parts.len() != 3 {
            return Err(malformed);
        }
        let header: TokenHeader = decode_part(parts[0])?;
        if header.alg != "RS256" {
            return Err(EntraError::Rejected(Reason::Algorithm));
        }
        let kid = header
            .kid
            .filter(|kid| !kid.is_empty() && kid.len() <= 128)
            .ok_or(malformed)?;
        let unverified: Unverified = decode_part(parts[1])?;
        let tenant = unverified
            .tid
            .as_deref()
            .and_then(|tid| Uuid::try_parse(tid).ok())
            .filter(|tenant| self.settings.tenants.admits(*tenant))
            .ok_or(EntraError::Rejected(Reason::Tenant))?;
        let issuer = self.settings.issuer(tenant);
        if unverified.iss.as_deref() != Some(issuer.as_str()) {
            return Err(EntraError::Rejected(Reason::Issuer));
        }
        let key = self
            .keys
            .key(tenant, &kid)
            .await?
            .ok_or(EntraError::Rejected(Reason::Signature))?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[issuer.as_str()]);
        validation.set_audience(&self.settings.audiences);
        validation.set_required_spec_claims(&["exp", "nbf", "iss", "aud"]);
        validation.validate_nbf = true;
        validation.leeway = LEEWAY_SECONDS;
        let claims = jsonwebtoken::decode::<claims::EntraClaims>(token, &key, &validation)
            .map_err(|error| EntraError::Rejected(reason_of(&error)))?
            .claims;
        claims::check(&claims, tenant, &self.settings).map_err(EntraError::Rejected)
    }
}

fn decode_part<T: DeserializeOwned>(part: &str) -> Result<T, EntraError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| EntraError::Rejected(Reason::Malformed))?;
    serde_json::from_slice(&bytes).map_err(|_| EntraError::Rejected(Reason::Malformed))
}

fn reason_of(error: &jsonwebtoken::errors::Error) -> Reason {
    use jsonwebtoken::errors::ErrorKind;
    match error.kind() {
        ErrorKind::ExpiredSignature => Reason::Expired,
        ErrorKind::ImmatureSignature => Reason::NotYetValid,
        ErrorKind::InvalidAudience => Reason::Audience,
        ErrorKind::InvalidIssuer => Reason::Issuer,
        ErrorKind::InvalidSignature => Reason::Signature,
        ErrorKind::InvalidAlgorithm => Reason::Algorithm,
        _ => Reason::Malformed,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::testing::{
        CLIENT_APP, KEY_A, KEY_B, OTHER_TENANT, StubOpenId, TENANT, sign, sign_as, unix_now,
        with_algorithm,
    };
    use super::*;

    const USER: &str = "55555555-0000-4000-8000-000000000005";

    fn quick() -> KeyTiming {
        KeyTiming {
            unknown_kid_cooldown: Duration::ZERO,
            retry_after_failure: Duration::ZERO,
            fetch_timeout: Duration::from_secs(2),
            ..KeyTiming::default()
        }
    }

    async fn verifier() -> (StubOpenId, EntraSignIn) {
        let stub = StubOpenId::start().await;
        let entra = EntraSignIn::with_timing(stub.settings(), quick()).unwrap();
        (stub, entra)
    }

    fn changed(mut claims: Value, name: &str, value: Value) -> Value {
        claims[name] = value;
        claims
    }

    fn without(mut claims: Value, name: &str) -> Value {
        claims.as_object_mut().unwrap().remove(name);
        claims
    }

    #[tokio::test]
    async fn a_delegated_token_for_this_api_is_accepted() {
        let (stub, entra) = verifier().await;
        let identity = entra.verify(&stub.token(USER)).await.unwrap();
        assert_eq!(identity.tenant_id.to_string(), TENANT);
        assert_eq!(identity.object_id.to_string(), USER);
        assert_eq!(identity.name.as_deref(), Some("Test User"));
        assert_eq!(identity.email.as_deref(), Some("test.user@contoso.test"));
        // Clock skew within the 60 s leeway.
        let early = changed(
            stub.user_claims(TENANT, USER),
            "nbf",
            json!(unix_now() + 50),
        );
        assert!(entra.verify(&sign(KEY_A, &early)).await.is_ok());
        let other = stub.user_claims(OTHER_TENANT, USER);
        let identity = entra.verify(&sign(KEY_A, &other)).await.unwrap();
        assert_eq!(identity.tenant_id.to_string(), OTHER_TENANT);
    }

    #[tokio::test]
    async fn every_kind_of_wrong_token_is_refused_with_its_reason() {
        let (stub, entra) = verifier().await;
        let good = stub.user_claims(TENANT, USER);
        let now = unix_now();
        let elsewhere = "66666666-0000-4000-8000-000000000006";
        let id_token = json!({
            "aud": CLIENT_APP, "iss": stub.issuer(TENANT), "iat": now, "nbf": now,
            "exp": now + 3600, "ver": "2.0", "tid": TENANT, "oid": USER, "sub": "pairwise",
            "nonce": "n-0S6_WzA2Mj", "name": "Test User",
            "preferred_username": "test.user@contoso.test",
        });
        let app_only = without(
            changed(good.clone(), "roles", json!(["Messages.Send"])),
            "scp",
        );
        let cases: Vec<(&str, String, Reason)> = vec![
            (
                "a Graph token",
                sign(
                    KEY_A,
                    &changed(
                        good.clone(),
                        "aud",
                        json!("00000003-0000-0000-c000-000000000000"),
                    ),
                ),
                Reason::Audience,
            ),
            (
                "an audience that is not configured",
                sign(
                    KEY_A,
                    &changed(good.clone(), "aud", json!("api://push-api")),
                ),
                Reason::Audience,
            ),
            (
                "a tenant that is not allowed",
                sign(
                    KEY_A,
                    &changed(
                        changed(good.clone(), "tid", json!(elsewhere)),
                        "iss",
                        json!(stub.issuer(elsewhere)),
                    ),
                ),
                Reason::Tenant,
            ),
            (
                "tid and iss disagree",
                sign(KEY_A, &changed(good.clone(), "tid", json!(OTHER_TENANT))),
                Reason::Issuer,
            ),
            (
                "the v1.0 issuer",
                sign(
                    KEY_A,
                    &changed(
                        good.clone(),
                        "iss",
                        json!(format!("https://sts.windows.net/{TENANT}/")),
                    ),
                ),
                Reason::Issuer,
            ),
            (
                "expired",
                sign(KEY_A, &changed(good.clone(), "exp", json!(now - 120))),
                Reason::Expired,
            ),
            (
                "not yet valid",
                sign(KEY_A, &changed(good.clone(), "nbf", json!(now + 300))),
                Reason::NotYetValid,
            ),
            (
                "a bad signature",
                sign_as(KEY_B, KEY_A.kid, &good),
                Reason::Signature,
            ),
            ("an unpublished key", sign(KEY_B, &good), Reason::Signature),
            (
                "alg none",
                with_algorithm("none", KEY_A.kid, &good, ""),
                Reason::Algorithm,
            ),
            (
                "alg HS256",
                with_algorithm("HS256", KEY_A.kid, &good, "c2lnbmF0dXJl"),
                Reason::Algorithm,
            ),
            (
                "alg RS384",
                with_algorithm("RS384", KEY_A.kid, &good, "c2lnbmF0dXJl"),
                Reason::Algorithm,
            ),
            (
                "version 1.0",
                sign(KEY_A, &changed(good.clone(), "ver", json!("1.0"))),
                Reason::Version,
            ),
            (
                "another scope",
                sign(KEY_A, &changed(good.clone(), "scp", json!("User.Read"))),
                Reason::Scope,
            ),
            ("app-only", sign(KEY_A, &app_only), Reason::AppOnly),
            (
                "app-only with idtyp",
                sign(KEY_A, &changed(good.clone(), "idtyp", json!("app"))),
                Reason::AppOnly,
            ),
            ("an ID token", sign(KEY_A, &id_token), Reason::Audience),
            (
                "no oid",
                sign(KEY_A, &without(good.clone(), "oid")),
                Reason::Subject,
            ),
            (
                "no exp",
                sign(KEY_A, &without(good.clone(), "exp")),
                Reason::Malformed,
            ),
            (
                "a string exp",
                sign(KEY_A, &changed(good.clone(), "exp", json!("tomorrow"))),
                Reason::Malformed,
            ),
            ("not a JWT", "not-a-token".to_owned(), Reason::Malformed),
            ("too long", "a.b.".repeat(3000), Reason::Malformed),
        ];
        for (case, token, reason) in cases {
            assert_eq!(
                entra.verify(&token).await,
                Err(EntraError::Rejected(reason)),
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn a_rotated_key_works_without_a_restart_and_an_outage_fails_closed() {
        let (stub, entra) = verifier().await;
        assert!(entra.verify(&stub.token(USER)).await.is_ok());
        stub.publish(&[KEY_B]);
        let rotated = sign(KEY_B, &stub.user_claims(TENANT, USER));
        assert!(entra.verify(&rotated).await.is_ok());

        let (down, cold) = verifier().await;
        down.set_failing(true);
        assert!(matches!(
            cold.verify(&down.token(USER)).await,
            Err(EntraError::Unavailable(_))
        ));
        // A token of a tenant that is not allowed never reaches the authority.
        let fetched = down.metadata_fetches();
        let foreign = "66666666-0000-4000-8000-000000000006";
        let claims = changed(
            changed(down.user_claims(TENANT, USER), "tid", json!(foreign)),
            "iss",
            json!(down.issuer(foreign)),
        );
        assert_eq!(
            cold.verify(&sign(KEY_A, &claims)).await,
            Err(EntraError::Rejected(Reason::Tenant))
        );
        assert_eq!(down.metadata_fetches(), fetched);
    }

    #[tokio::test]
    async fn each_keyword_admits_its_tenants_and_checks_each_token_against_its_own_tid() {
        let stub = StubOpenId::start().await;
        let work = "77777777-0000-4000-8000-000000000007";
        let personal = CONSUMERS_TENANT.to_string();
        let verifier = |tenants: Tenants| {
            let settings = EntraSettings {
                tenants,
                ..stub.settings()
            };
            EntraSignIn::with_timing(settings, quick()).unwrap()
        };
        let token = |tenant: &str| sign(KEY_A, &stub.user_claims(tenant, USER));
        for (tenants, work_ok, personal_ok) in [
            (Tenants::Common, true, true),
            (Tenants::Organizations, true, false),
            (Tenants::Consumers, false, true),
        ] {
            let entra = verifier(tenants.clone());
            for (tenant, admitted) in [(work, work_ok), (personal.as_str(), personal_ok)] {
                let verified = entra.verify(&token(tenant)).await;
                if admitted {
                    assert_eq!(
                        verified.unwrap().tenant_id.to_string(),
                        tenant,
                        "{tenants:?}"
                    );
                } else {
                    assert_eq!(
                        verified,
                        Err(EntraError::Rejected(Reason::Tenant)),
                        "{tenants:?}"
                    );
                }
            }
            // The issuer must still name the token's own tenant.
            let other_issuer = changed(
                stub.user_claims(work, USER),
                "iss",
                json!(stub.issuer(TENANT)),
            );
            if tenants.admits(Uuid::parse_str(work).unwrap()) {
                assert_eq!(
                    entra.verify(&sign(KEY_A, &other_issuer)).await,
                    Err(EntraError::Rejected(Reason::Issuer))
                );
            }
        }
    }
}
