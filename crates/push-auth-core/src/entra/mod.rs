//! Microsoft Entra ID sign-in (spec 2026-10-19): auth-svc checks an Entra ID access token
//! issued for its API and finds the user by the token's immutable `(tid, oid)`. It never
//! redeems authorization codes and holds no client secret. Tokens, object ids, names and
//! email addresses are never logged.

mod app;
mod claims;
mod keys;
mod settings;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod validator;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, Validation};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::identity::{AuthenticationError, Authenticator, ENTRA_PROVIDER, Principal};

pub use app::{EntraAppSettings, EntraAppSignIn};
pub use keys::KeyTiming;
pub use settings::{
    CONSUMERS_TENANT, DEFAULT_AUTHORITY_HOST, DEFAULT_REQUIRED_SCOPE, EntraSettings, Providers,
    Tenants,
};
use validator::ProviderKind;
pub use validator::{MAX_VALIDATOR_BODY, ValidatorSettings};

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
    /// The external token validator refused the token without a known reason.
    Validator,
}

impl Reason {
    const ALL: [Reason; 15] = [
        Self::Malformed,
        Self::Algorithm,
        Self::Tenant,
        Self::Issuer,
        Self::Signature,
        Self::Audience,
        Self::Expired,
        Self::NotYetValid,
        Self::Version,
        Self::AppOnly,
        Self::Scope,
        Self::Role,
        Self::ClientApp,
        Self::Subject,
        Self::Validator,
    ];

    /// The reason named `name` in the security log, if there is one.
    pub fn from_name(name: &str) -> Option<Reason> {
        Self::ALL.into_iter().find(|reason| reason.as_str() == name)
    }

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
            Self::Validator => "validator",
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

impl EntraIdentity {
    /// The principal this identity is (spec D2): provider `entra`, the tenant id as the
    /// issuer, the object id as the subject, with the token's verified `claims`.
    pub fn principal(&self, claims: Map<String, Value>) -> Principal {
        Principal {
            provider: ENTRA_PROVIDER.to_owned(),
            issuer: self.tenant_id.to_string(),
            subject: self.object_id.to_string(),
            name: self.name.clone(),
            email: self.email.clone(),
            claims,
        }
    }
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
/// within 60 s, then the claim rules. With an external token validator configured, the
/// validator checks the signature instead and its claims are checked again; no keys are
/// fetched then.
#[derive(Clone)]
pub struct EntraSignIn {
    settings: Arc<EntraSettings>,
    /// The published keys: set exactly when there is no validator.
    keys: Option<Arc<keys::KeySets>>,
    validator: Option<Arc<validator::Validator>>,
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
        let (keys, validator) = match &settings.validator {
            Some(validator) => (None, Some(Arc::new(validator::Validator::new(validator)?))),
            None => (Some(Arc::new(keys::KeySets::new(&settings, timing)?)), None),
        };
        Ok(Self {
            settings: Arc::new(settings),
            keys,
            validator,
        })
    }

    /// Whether an external token validator checks the tokens.
    pub fn uses_validator(&self) -> bool {
        self.validator.is_some()
    }

    /// The security log's `validator` field: `true` in validator mode, absent otherwise.
    fn validator_flag(&self) -> Option<bool> {
        self.uses_validator().then_some(true)
    }

    /// Logs that no identity could be checked, without the token.
    fn log_unavailable(&self, error: &str, provider: &str) {
        match (self.uses_validator(), provider) {
            (false, ENTRA_PROVIDER) => {
                tracing::warn!(%error, "an Entra ID sign-in failed: no usable signing keys");
            }
            (false, _) => {
                tracing::warn!(%error, "an Entra app sign-in failed: no usable signing keys");
            }
            (true, ENTRA_PROVIDER) => tracing::warn!(
                %error,
                "an Entra ID sign-in failed: the token validator is unavailable"
            ),
            (true, _) => tracing::warn!(
                %error,
                "an Entra app sign-in failed: the token validator is unavailable"
            ),
        }
    }

    pub fn settings(&self) -> &EntraSettings {
        &self.settings
    }

    /// How long an Entra-backed device stays signed in (spec D8).
    pub fn max_session(&self) -> Duration {
        self.settings.max_session
    }

    /// Fetches every allowed tenant's keys once; failures are logged. Does nothing with a
    /// token validator: it owns key discovery.
    pub async fn warm(&self) {
        if let Some(keys) = &self.keys {
            keys.warm().await;
        }
    }

    /// The user `token` names, if it is an acceptable access token for this API. A
    /// refusal is logged with its reason, never with the token or its claims.
    pub async fn verify(&self, token: &str) -> Result<EntraIdentity, EntraError> {
        let checked = self.check(token).await.map(|(identity, _)| identity);
        match &checked {
            Err(EntraError::Rejected(reason)) => tracing::warn!(
                target: "security",
                event = "auth.entra.rejected",
                reason = reason.as_str(),
                validator = self.validator_flag(),
                "an Entra ID access token was refused"
            ),
            Err(EntraError::Unavailable(error)) => self.log_unavailable(error, ENTRA_PROVIDER),
            Ok(_) => {}
        }
        checked
    }

    /// The identity of an acceptable token, and the token's whole verified payload.
    async fn check(&self, token: &str) -> Result<(EntraIdentity, Map<String, Value>), EntraError> {
        let (claims, tenant, payload) = self.verified(token, ProviderKind::User).await?;
        let identity =
            claims::check(&claims, tenant, &self.settings).map_err(EntraError::Rejected)?;
        Ok((identity, payload))
    }

    /// Cryptographic and common claim checks, shared by delegated and app-only policy.
    async fn verified(
        &self,
        token: &str,
        kind: ProviderKind,
    ) -> Result<(claims::EntraClaims, Uuid, Map<String, Value>), EntraError> {
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
        let payload = match (&self.validator, &self.keys) {
            (Some(validator), _) => {
                // Only a base64url signature can travel in the Authorization header.
                let signature = parts[2];
                if signature.is_empty()
                    || !signature
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
                {
                    return Err(EntraError::Rejected(Reason::Malformed));
                }
                let payload = validator.check(token, kind).await?;
                // Any JSON type: only a string can match the validator's `oid`.
                let unverified_claims: Map<String, Value> = decode_part(parts[1])?;
                validator::recheck(
                    &payload,
                    unverified.tid.as_deref(),
                    unverified_claims.get("oid").and_then(Value::as_str),
                    tenant,
                    &self.settings,
                    crate::service::unix_now(),
                )
                .map_err(EntraError::Rejected)?;
                payload
            }
            (None, Some(keys)) => {
                let key = keys
                    .key(tenant, &kid)
                    .await?
                    .ok_or(EntraError::Rejected(Reason::Signature))?;
                let mut validation = Validation::new(Algorithm::RS256);
                validation.set_issuer(&[issuer.as_str()]);
                validation.set_audience(&self.settings.audiences);
                validation.set_required_spec_claims(&["exp", "nbf", "iss", "aud"]);
                validation.validate_nbf = true;
                validation.leeway = LEEWAY_SECONDS;
                jsonwebtoken::decode::<Map<String, Value>>(token, &key, &validation)
                    .map_err(|error| EntraError::Rejected(reason_of(&error)))?
                    .claims
            }
            (None, None) => unreachable!("an Entra verifier has keys or a validator"),
        };
        // A claim of the wrong JSON type makes the whole token malformed, as before.
        let claims: claims::EntraClaims = serde_json::from_value(Value::Object(payload.clone()))
            .map_err(|_| EntraError::Rejected(Reason::Malformed))?;
        Ok((claims, tenant, payload))
    }
}

/// The built-in Microsoft Entra ID check as an [`Authenticator`] (spec D3): provider
/// `entra`. A refusal is logged by the identity middleware, not here.
#[async_trait]
impl Authenticator for EntraSignIn {
    fn provider(&self) -> &str {
        ENTRA_PROVIDER
    }

    fn max_session(&self) -> Duration {
        self.settings.max_session
    }

    async fn authenticate(&self, token: &str) -> Result<Principal, AuthenticationError> {
        match self.check(token).await {
            Ok((identity, claims)) => Ok(identity.principal(claims)),
            Err(EntraError::Rejected(reason)) => {
                Err(AuthenticationError::Rejected(reason.as_str()))
            }
            Err(EntraError::Unavailable(error)) => {
                self.log_unavailable(&error, ENTRA_PROVIDER);
                Err(AuthenticationError::Unavailable(error))
            }
        }
    }

    async fn warm(&self) {
        EntraSignIn::warm(self).await;
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
        API_CLIENT_ID, CLIENT_APP, KEY_A, KEY_B, OTHER_TENANT, StubOpenId, StubValidator, TENANT,
        ValidatorAnswer, refused_url, sign, sign_as, sign_raw, unix_now, with_algorithm,
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
    async fn a_built_in_check_takes_the_last_of_duplicated_oid_claims() {
        let (stub, entra) = verifier().await;
        let first = "66666666-0000-4000-8000-000000000006";
        let claims = without(stub.user_claims(TENANT, USER), "oid");
        let body = claims.to_string();
        let payload = format!(
            "{{\"oid\":\"{first}\",{},\"oid\":\"{USER}\"}}",
            &body[1..body.len() - 1]
        );
        let identity = entra.verify(&sign_raw(KEY_A, &payload)).await.unwrap();
        assert_eq!(identity.object_id.to_string(), USER);
    }

    #[tokio::test]
    async fn as_an_authenticator_it_names_the_tenant_and_object_id_with_every_claim() {
        use crate::identity::{AuthenticationError, Authenticator};

        let (stub, entra) = verifier().await;
        let checker: &dyn Authenticator = &entra;
        assert_eq!(checker.provider(), "entra");
        assert_eq!(checker.max_session(), Duration::from_secs(24 * 3600));
        let principal = checker.authenticate(&stub.token(USER)).await.unwrap();
        assert_eq!(principal.provider, "entra");
        assert_eq!(principal.issuer, TENANT);
        assert_eq!(principal.subject, USER);
        assert_eq!(principal.name.as_deref(), Some("Test User"));
        assert_eq!(principal.email.as_deref(), Some("test.user@contoso.test"));
        assert_eq!(principal.claims["scp"], "access_as_user");
        assert_eq!(principal.claims["azp"], CLIENT_APP);
        let shown = format!("{principal:?}");
        assert!(shown.contains(TENANT) && !shown.contains(USER), "{shown}");
        assert!(!shown.contains("Test User"), "{shown}");

        let mut claims = stub.user_claims(TENANT, USER);
        claims["aud"] = json!(CLIENT_APP);
        assert_eq!(
            checker.authenticate(&sign(KEY_A, &claims)).await,
            Err(AuthenticationError::Rejected("audience"))
        );
        let (down, cold) = verifier().await;
        down.set_failing(true);
        assert!(matches!(
            cold.authenticate(&down.token(USER)).await,
            Err(AuthenticationError::Unavailable(_))
        ));
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

    async fn validated() -> (StubOpenId, StubValidator, EntraSignIn) {
        let openid = StubOpenId::start().await;
        let validator = StubValidator::start().await;
        let entra = EntraSignIn::new(validator.settings_for(&openid)).unwrap();
        (openid, validator, entra)
    }

    #[test]
    fn every_reason_is_found_by_its_name() {
        for reason in Reason::ALL {
            assert_eq!(Reason::from_name(reason.as_str()), Some(reason));
        }
        assert_eq!(Reason::Validator.as_str(), "validator");
        assert_eq!(
            Reason::from_name("not_yet_valid"),
            Some(Reason::NotYetValid)
        );
        assert_eq!(Reason::from_name("bogus"), None);
        assert_eq!(Reason::from_name("Signature"), None);
    }

    #[tokio::test]
    async fn without_a_validator_url_no_validator_is_asked() {
        let (openid, entra) = verifier().await;
        let validator = StubValidator::start().await;
        assert!(!entra.uses_validator());
        assert!(entra.verify(&openid.token(USER)).await.is_ok());
        assert!(validator.requests().is_empty());
    }

    #[tokio::test]
    async fn a_validated_delegated_token_gives_the_built_in_principal() {
        use crate::identity::Authenticator;

        let (openid, validator, entra) = validated().await;
        assert!(entra.uses_validator());
        let token = openid.token(USER);
        let checked = Authenticator::authenticate(&entra, &token).await.unwrap();
        assert_eq!((openid.metadata_fetches(), openid.jwks_fetches()), (0, 0));
        assert_eq!(validator.requests().len(), 1);
        let built_in = EntraSignIn::with_timing(openid.settings(), quick()).unwrap();
        let expected = Authenticator::authenticate(&built_in, &token)
            .await
            .unwrap();
        assert!(checked == expected, "{checked:?} != {expected:?}");
        assert_eq!(checked.subject, USER);
        // A validator may omit the optional nbf.
        validator.set_answer(ValidatorAnswer::Claims(without(
            openid.user_claims(TENANT, USER),
            "nbf",
        )));
        assert!(entra.verify(&token).await.is_ok());
    }

    #[tokio::test]
    async fn the_validators_claims_are_checked_again() {
        let (openid, validator, entra) = validated().await;
        let good = openid.user_claims(TENANT, USER);
        let now = unix_now();
        let cases = [
            ("aud", json!(CLIENT_APP), Reason::Audience),
            ("aud", json!(["api://other", CLIENT_APP]), Reason::Audience),
            ("iss", json!(openid.issuer(OTHER_TENANT)), Reason::Issuer),
            ("exp", json!(now - 120), Reason::Expired),
            ("nbf", json!(now + 300), Reason::NotYetValid),
            ("tid", json!(OTHER_TENANT), Reason::Malformed),
            (
                "oid",
                json!("66666666-0000-4000-8000-000000000006"),
                Reason::Malformed,
            ),
            ("ver", json!("1.0"), Reason::Version),
            ("scp", json!("User.Read"), Reason::Scope),
        ];
        for (name, value, reason) in cases {
            validator.set_answer(ValidatorAnswer::Claims(changed(good.clone(), name, value)));
            assert_eq!(
                entra.verify(&openid.token(USER)).await,
                Err(EntraError::Rejected(reason)),
                "{name}"
            );
        }
        validator.set_answer(ValidatorAnswer::Claims(changed(
            good.clone(),
            "aud",
            json!(["api://other", API_CLIENT_ID]),
        )));
        assert!(entra.verify(&openid.token(USER)).await.is_ok());
        // A token without oid cannot match the validator's.
        validator.set_answer(ValidatorAnswer::Claims(good.clone()));
        let no_oid = sign(KEY_A, &without(good.clone(), "oid"));
        assert_eq!(
            entra.verify(&no_oid).await,
            Err(EntraError::Rejected(Reason::Malformed))
        );

        // A tenant that is not admitted never reaches the validator.
        let asked = validator.requests().len();
        let foreign = "66666666-0000-4000-8000-000000000006";
        let claims = changed(
            changed(good.clone(), "tid", json!(foreign)),
            "iss",
            json!(openid.issuer(foreign)),
        );
        assert_eq!(
            entra.verify(&sign(KEY_A, &claims)).await,
            Err(EntraError::Rejected(Reason::Tenant))
        );
        for token in [
            with_algorithm("none", KEY_A.kid, &good, ""),
            with_algorithm("HS256", KEY_A.kid, &good, "c2lnbmF0dXJl"),
        ] {
            assert_eq!(
                entra.verify(&token).await,
                Err(EntraError::Rejected(Reason::Algorithm))
            );
        }
        assert_eq!(validator.requests().len(), asked);
    }

    #[tokio::test]
    async fn a_signature_that_is_not_base64url_never_reaches_the_validator() {
        let (openid, validator, entra) = validated().await;
        let good = openid.user_claims(TENANT, USER);
        for signature in ["ab\u{1}cd", "", "ab cd", "ab+cd", "ab/cd", "abc="] {
            let token = with_algorithm("RS256", KEY_A.kid, &good, signature);
            assert_eq!(
                entra.verify(&token).await,
                Err(EntraError::Rejected(Reason::Malformed)),
                "{signature:?}"
            );
        }
        assert!(validator.requests().is_empty());
    }

    #[tokio::test]
    async fn a_chunked_validator_answer_over_16_kib_is_unavailable() {
        let (openid, validator, entra) = validated().await;
        let mut big = openid.user_claims(TENANT, USER);
        big["name"] = json!("n".repeat(17 * 1024));
        validator.set_answer(ValidatorAnswer::Chunked(
            json!({ "claims": big }).to_string(),
        ));
        let verified = entra.verify(&openid.token(USER)).await;
        assert!(
            matches!(&verified, Err(EntraError::Unavailable(error)) if error.contains("16 KiB")),
            "{verified:?}"
        );
        // The same answer under the cap is read.
        validator.set_answer(ValidatorAnswer::Chunked(
            json!({ "claims": openid.user_claims(TENANT, USER) }).to_string(),
        ));
        assert!(entra.verify(&openid.token(USER)).await.is_ok());
        assert_eq!(validator.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_validator_answer_off_the_schema_is_malformed() {
        let (openid, validator, entra) = validated().await;
        let good = openid.user_claims(TENANT, USER);
        let answers = [
            ValidatorAnswer::Status(200, r#"{"other":{}}"#.to_owned()),
            ValidatorAnswer::Status(200, r#"{"claims":"x"}"#.to_owned()),
            ValidatorAnswer::Status(200, "[1]".to_owned()),
            ValidatorAnswer::Claims(without(good.clone(), "exp")),
            ValidatorAnswer::Claims(without(good.clone(), "ver")),
            ValidatorAnswer::Claims(changed(good.clone(), "exp", json!("tomorrow"))),
            ValidatorAnswer::Claims(changed(good.clone(), "exp", json!(1.5))),
            ValidatorAnswer::Claims(changed(good.clone(), "nbf", json!(1.5))),
            ValidatorAnswer::Claims(changed(good.clone(), "aud", json!(5))),
            ValidatorAnswer::Claims(changed(good.clone(), "aud", json!([API_CLIENT_ID, 5]))),
            ValidatorAnswer::Claims(changed(good.clone(), "roles", json!("a"))),
            ValidatorAnswer::Claims(changed(good.clone(), "roles", json!([1]))),
            ValidatorAnswer::Claims(changed(good.clone(), "name", Value::Null)),
            ValidatorAnswer::Claims(changed(good.clone(), "appid", json!(1))),
        ];
        for answer in answers {
            validator.set_answer(answer.clone());
            assert_eq!(
                entra.verify(&openid.token(USER)).await,
                Err(EntraError::Rejected(Reason::Malformed)),
                "{answer:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_401_names_the_reason_or_the_validator() {
        let (openid, validator, entra) = validated().await;
        for (body, reason) in [
            (
                r#"{"error":"invalid_token","reason":"signature"}"#,
                Reason::Signature,
            ),
            (r#"{"reason":"expired"}"#, Reason::Expired),
            ("", Reason::Validator),
            (r#"{"reason":"bogus"}"#, Reason::Validator),
            (r#"{"reason":5}"#, Reason::Validator),
            (r#"{"error":"invalid_token"}"#, Reason::Validator),
            ("not json", Reason::Validator),
        ] {
            validator.set_answer(ValidatorAnswer::Status(401, body.to_owned()));
            assert_eq!(
                entra.verify(&openid.token(USER)).await,
                Err(EntraError::Rejected(reason)),
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn an_unusable_validator_fails_closed_without_falling_back_to_keys() {
        use crate::identity::{AuthenticationError, Authenticator};

        let (openid, validator, entra) = validated().await;
        let token = openid.token(USER);
        let mut big = openid.user_claims(TENANT, USER);
        big["name"] = json!("n".repeat(17_000));
        for answer in [
            ValidatorAnswer::Status(403, r#"{"error":"forbidden"}"#.to_owned()),
            ValidatorAnswer::Status(500, String::new()),
            ValidatorAnswer::Status(302, String::new()),
            ValidatorAnswer::Status(400, String::new()),
            ValidatorAnswer::Status(401, "x".repeat(17 * 1024)),
            ValidatorAnswer::Status(200, "not json".to_owned()),
            ValidatorAnswer::Claims(big),
            ValidatorAnswer::Delay(Duration::from_millis(1500)),
        ] {
            validator.set_answer(answer.clone());
            let verified = entra.verify(&token).await;
            let Err(EntraError::Unavailable(error)) = verified else {
                panic!("{answer:?}: {verified:?}");
            };
            assert!(!error.contains(&token), "{error}");
            assert!(
                matches!(
                    Authenticator::authenticate(&entra, &token).await,
                    Err(AuthenticationError::Unavailable(_))
                ),
                "{answer:?}"
            );
        }
        let refused = EntraSignIn::new(EntraSettings {
            validator: Some(ValidatorSettings {
                url: refused_url().await,
                timeout: Duration::from_millis(1000),
                secret: None,
            }),
            ..openid.settings()
        })
        .unwrap();
        let verified = refused.verify(&token).await;
        // Windows retries a refused connection for longer than the timeout.
        assert!(
            matches!(&verified, Err(EntraError::Unavailable(error))
                if error.contains("could not be reached") || error.contains("timed out")),
            "{verified:?}"
        );
        entra.warm().await;
        Authenticator::warm(&refused).await;
        assert_eq!((openid.metadata_fetches(), openid.jwks_fetches()), (0, 0));
    }

    #[tokio::test]
    async fn the_validator_request_carries_the_token_in_the_header_only() {
        let (openid, validator, entra) = validated().await;
        let token = openid.token(USER);
        entra.verify(&token).await.unwrap();
        entra.verify(&token).await.unwrap();
        let requests = validator.requests();
        assert_eq!(requests.len(), 2, "no caching");
        let seen = &requests[0];
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.authorization, Some(format!("Bearer {token}")));
        assert_eq!(
            serde_json::from_str::<Value>(&seen.body).unwrap(),
            json!({"provider": "entra"})
        );
        assert!(!seen.body.contains(&token));
        assert_eq!(seen.content_type.as_deref(), Some("application/json"));
        assert_eq!(seen.accept.as_deref(), Some("application/json"));
        assert_eq!(seen.secret, None);

        let mut settings = validator.settings_for(&openid);
        settings.validator.as_mut().unwrap().secret = Some("s3cret".to_owned());
        let with_secret = EntraSignIn::new(settings).unwrap();
        with_secret.verify(&token).await.unwrap();
        assert_eq!(validator.requests()[2].secret.as_deref(), Some("s3cret"));
        assert!(!format!("{with_secret:?}").contains("s3cret"));
    }
}
