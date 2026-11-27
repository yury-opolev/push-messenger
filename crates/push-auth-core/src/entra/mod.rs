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

use crate::identity::{AuthenticationError, Authenticator, ENTRA_PROVIDER, Principal, RequestMeta};

pub use app::{EntraAppSettings, EntraAppSignIn};
pub use keys::KeyTiming;
pub use settings::{
    CONSUMERS_TENANT, DEFAULT_AUTHORITY_HOST, DEFAULT_REQUIRED_SCOPE, EntraSettings, Providers,
    Tenants,
};
use validator::ProviderKind;
pub use validator::{MAX_VALIDATOR_BODY, Protocol, ValidatorSettings};

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
        let checked = self
            .check(token, &RequestMeta::default())
            .await
            .map(|(identity, _)| identity);
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
    async fn check(
        &self,
        token: &str,
        meta: &RequestMeta,
    ) -> Result<(EntraIdentity, Map<String, Value>), EntraError> {
        let (claims, tenant, payload) = self.verified(token, ProviderKind::User, meta).await?;
        let identity =
            claims::check(&claims, tenant, &self.settings).map_err(EntraError::Rejected)?;
        Ok((identity, payload))
    }

    /// Cryptographic and common claim checks, shared by delegated and app-only policy.
    /// `meta` describes the sign-in request to a validator.
    async fn verified(
        &self,
        token: &str,
        kind: ProviderKind,
        meta: &RequestMeta,
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
                let payload = validator.check(token, kind, meta).await?;
                // Any JSON type: only a string can match the validator's `oid` or `sub`.
                let unverified_claims: Map<String, Value> = decode_part(parts[1])?;
                let text = |name: &str| unverified_claims.get(name).and_then(Value::as_str);
                validator::recheck(
                    &payload,
                    validator::Presented {
                        tid: unverified.tid.as_deref(),
                        oid: text("oid"),
                        sub: text("sub"),
                    },
                    validator.requires_version(),
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
        self.authenticate_with(token, &RequestMeta::default()).await
    }

    async fn authenticate_with(
        &self,
        token: &str,
        meta: &RequestMeta,
    ) -> Result<Principal, AuthenticationError> {
        match self.check(token, meta).await {
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
        ValidatorAnswer, claim_headers, refused_url, sign, sign_as, sign_raw, unix_now,
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
            ("sub", json!("pairwise-someone-else"), Reason::Malformed),
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
        // A validator may omit sub, but not name another one than the token's.
        validator.set_answer(ValidatorAnswer::Claims(without(good.clone(), "sub")));
        assert!(entra.verify(&openid.token(USER)).await.is_ok());
        validator.set_answer(ValidatorAnswer::Claims(good.clone()));
        let no_sub = sign(KEY_A, &without(good.clone(), "sub"));
        assert_eq!(
            entra.verify(&no_sub).await,
            Err(EntraError::Rejected(Reason::Malformed))
        );
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
                timeout: Duration::from_millis(1000),
                ..ValidatorSettings::new(refused_url().await)
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

    const REQUEST_ID: &str = "01937e4c-9c1a-7000-8000-000000000001";

    fn meta() -> RequestMeta {
        RequestMeta {
            original_uri: Some("http://push.local/v1/auth/entra".to_owned()),
            method: Some("POST".to_owned()),
            client_ip: "198.51.100.7".parse().ok(),
            request_id: Some(REQUEST_ID.to_owned()),
        }
    }

    /// `seen`'s single value of the header `name`, if any.
    fn single<'a>(seen: &'a super::testing::SeenRequest, name: &str) -> Option<&'a str> {
        let values = seen.header(name);
        assert!(values.len() <= 1, "{name}: {values:?}");
        values.first().copied()
    }

    /// Checks that `seen` carried `meta()`, the static header `X-Tier: gold` and no secret.
    fn assert_carries_the_metadata(seen: &super::testing::SeenRequest, token: &str) {
        let authorization = format!("Bearer {token}");
        assert_eq!(seen.method, "POST");
        assert_eq!(single(seen, "authorization"), Some(authorization.as_str()));
        assert_eq!(
            single(seen, "original-uri"),
            Some("http://push.local/v1/auth/entra")
        );
        assert_eq!(single(seen, "original-method"), Some("POST"));
        assert_eq!(single(seen, "x-forwarded-for"), Some("198.51.100.7"));
        assert_eq!(single(seen, "x-request-id"), Some(REQUEST_ID));
        assert_eq!(single(seen, "x-tier"), Some("gold"));
        assert_eq!(single(seen, "x-push-validator-secret"), None);
        let elsewhere: Vec<_> = seen
            .headers
            .iter()
            .filter(|(name, value)| name != "authorization" && value.contains(token))
            .collect();
        assert!(elsewhere.is_empty(), "{elsewhere:?}");
    }

    fn with_static_header(mut settings: EntraSettings) -> EntraSettings {
        let validator = settings.validator.as_mut().unwrap();
        validator.request_headers.push((
            reqwest::header::HeaderName::from_static("x-tier"),
            reqwest::header::HeaderValue::from_static("gold"),
        ));
        settings
    }

    #[tokio::test]
    async fn a_json_validator_request_carries_the_request_metadata() {
        use crate::identity::Authenticator;

        let openid = StubOpenId::start().await;
        let validator = StubValidator::start().await;
        let entra = EntraSignIn::new(with_static_header(validator.settings_for(&openid))).unwrap();
        let token = openid.token(USER);
        entra.authenticate_with(&token, &meta()).await.unwrap();
        let seen = &validator.requests()[0];
        assert_carries_the_metadata(seen, &token);
        assert_eq!(seen.content_type.as_deref(), Some("application/json"));
        assert_eq!(
            serde_json::from_str::<Value>(&seen.body).unwrap(),
            json!({"provider": "entra"})
        );

        // Unknown or unsafe metadata is not sent; neither is a disabled request id.
        let mut settings = with_static_header(validator.settings_for(&openid));
        settings.validator.as_mut().unwrap().correlation_header = None;
        let quiet = EntraSignIn::new(settings).unwrap();
        quiet.authenticate_with(&token, &meta()).await.unwrap();
        let unsafe_meta = RequestMeta {
            original_uri: Some("http://push.local/v1/auth/entra\u{e9}".to_owned()),
            method: Some("PO ST".to_owned()),
            client_ip: None,
            request_id: Some("a\r\nb".to_owned()),
        };
        entra.authenticate_with(&token, &unsafe_meta).await.unwrap();
        entra.verify(&token).await.unwrap();
        let requests = validator.requests();
        assert_eq!(single(&requests[1], "x-request-id"), None);
        assert_eq!(single(&requests[1], "original-method"), Some("POST"));
        for seen in &requests[2..] {
            for name in [
                "original-uri",
                "original-method",
                "x-forwarded-for",
                "x-request-id",
            ] {
                assert_eq!(single(seen, name), None, "{name}: {seen:?}");
            }
        }
        for seen in &requests {
            assert_eq!(single(seen, "x-tier"), Some("gold"));
        }
    }

    async fn headers_mode() -> (StubOpenId, StubValidator, EntraSignIn) {
        let openid = StubOpenId::start().await;
        let validator = StubValidator::start().await;
        let entra = EntraSignIn::new(validator.settings_for_headers(&openid)).unwrap();
        (openid, validator, entra)
    }

    /// A header-protocol answer: 200 with the claim headers of `claims`.
    fn headers_answer(claims: &Value) -> ValidatorAnswer {
        ValidatorAnswer::Headers {
            status: 200,
            headers: claim_headers(claims),
        }
    }

    #[tokio::test]
    async fn a_header_validator_request_is_an_empty_post_with_the_metadata() {
        use crate::identity::Authenticator;

        let openid = StubOpenId::start().await;
        let validator = StubValidator::start().await;
        let mut settings = with_static_header(validator.settings_for_headers(&openid));
        let entra = EntraSignIn::new(settings.clone()).unwrap();
        let token = openid.token(USER);
        entra.authenticate_with(&token, &meta()).await.unwrap();
        let requests = validator.requests();
        assert_eq!(requests.len(), 1);
        let seen = &requests[0];
        assert_carries_the_metadata(seen, &token);
        assert_eq!(seen.body, "");
        assert_eq!(seen.content_length.as_deref(), Some("0"));
        assert_eq!(seen.content_type, None);
        assert_eq!(single(seen, "transfer-encoding"), None);
        assert_eq!(single(seen, "return-all-subject-token-claims"), Some("1"));

        let configured = settings.validator.as_mut().unwrap();
        configured.secret = Some("s3cret".to_owned());
        configured.correlation_header =
            Some(reqwest::header::HeaderName::from_static("x-correlation-id"));
        let with_secret = EntraSignIn::new(settings).unwrap();
        with_secret
            .authenticate_with(&token, &meta())
            .await
            .unwrap();
        let seen = &validator.requests()[1];
        assert_eq!(seen.secret.as_deref(), Some("s3cret"));
        assert_eq!(single(seen, "x-correlation-id"), Some(REQUEST_ID));
        assert_eq!(single(seen, "x-request-id"), None);
        assert!(!format!("{with_secret:?}").contains("s3cret"));
    }

    #[tokio::test]
    async fn a_header_validated_delegated_token_gives_the_built_in_principal() {
        use crate::identity::Authenticator;

        let (openid, validator, entra) = headers_mode().await;
        let token = openid.token(USER);
        let checked = Authenticator::authenticate(&entra, &token).await.unwrap();
        assert_eq!((openid.metadata_fetches(), openid.jwks_fetches()), (0, 0));
        assert_eq!(validator.requests().len(), 1);
        let built_in = EntraSignIn::with_timing(openid.settings(), quick()).unwrap();
        let expected = Authenticator::authenticate(&built_in, &token)
            .await
            .unwrap();
        assert!(checked == expected, "{checked:?} != {expected:?}");
        assert_eq!(checked.claims, expected.claims);
        assert_eq!(checked.claims["exp"], expected.claims["exp"]);
        assert_eq!(checked.name.as_deref(), Some("Test User"));

        let name = "Zo\u{eb} \u{c5}ngstr\u{f6}m \u{6d4b}\u{8bd5}";
        let claims = changed(openid.user_claims(TENANT, USER), "name", json!(name));
        let identity = entra.verify(&sign(KEY_A, &claims)).await.unwrap();
        assert_eq!(identity.name.as_deref(), Some(name));
        // A validator may omit nbf and iat.
        let short = without(without(openid.user_claims(TENANT, USER), "nbf"), "iat");
        validator.set_answer(headers_answer(&short));
        assert!(entra.verify(&token).await.is_ok());
        assert_eq!(validator.requests().len(), 3, "one request per sign-in");
    }

    #[tokio::test]
    async fn header_claims_are_checked_again() {
        let (openid, validator, entra) = headers_mode().await;
        let good = openid.user_claims(TENANT, USER);
        let token = openid.token(USER);
        let now = unix_now();
        let mut cases: Vec<(Value, Reason)> = ["tid", "oid", "exp", "iss", "aud"]
            .into_iter()
            .map(|missing| (without(good.clone(), missing), Reason::Malformed))
            .collect();
        cases.extend([
            (without(good.clone(), "ver"), Reason::Version),
            (changed(good.clone(), "ver", json!("1.0")), Reason::Version),
            (
                changed(good.clone(), "tid", json!(OTHER_TENANT)),
                Reason::Malformed,
            ),
            (
                changed(
                    good.clone(),
                    "oid",
                    json!("66666666-0000-4000-8000-000000000006"),
                ),
                Reason::Malformed,
            ),
            (
                changed(good.clone(), "sub", json!("pairwise-other")),
                Reason::Malformed,
            ),
            (
                changed(good.clone(), "exp", json!("tomorrow")),
                Reason::Malformed,
            ),
            (changed(good.clone(), "exp", json!(1.5)), Reason::Malformed),
            (
                changed(good.clone(), "aud", json!(CLIENT_APP)),
                Reason::Audience,
            ),
            (
                changed(good.clone(), "aud", json!(["api://other", CLIENT_APP])),
                Reason::Audience,
            ),
            (
                changed(good.clone(), "iss", json!(openid.issuer(OTHER_TENANT))),
                Reason::Issuer,
            ),
            (
                changed(good.clone(), "exp", json!(now - 120)),
                Reason::Expired,
            ),
            (
                changed(good.clone(), "nbf", json!(now + 300)),
                Reason::NotYetValid,
            ),
            (changed(good.clone(), "scp", json!("other")), Reason::Scope),
        ]);
        for (claims, reason) in cases {
            validator.set_answer(headers_answer(&claims));
            assert_eq!(
                entra.verify(&token).await,
                Err(EntraError::Rejected(reason)),
                "{claims}"
            );
        }
        let mut repeated = claim_headers(&good);
        repeated.push(("Subject-Token-Claim-oid".to_owned(), USER.to_owned()));
        let mut bad_base64 = claim_headers(&good);
        bad_base64.push((
            "Subject-Token-Encoded-Claim-email".to_owned(),
            "%%%".to_owned(),
        ));
        for headers in [repeated, bad_base64] {
            validator.set_answer(ValidatorAnswer::Headers {
                status: 200,
                headers,
            });
            assert_eq!(
                entra.verify(&token).await,
                Err(EntraError::Rejected(Reason::Malformed))
            );
        }
        // Several audiences, one of them configured, in any of the list forms.
        for audiences in [
            format!("api://other,{API_CLIENT_ID}"),
            format!("[\"api://other\",\"{API_CLIENT_ID}\"]"),
        ] {
            let mut headers = claim_headers(&without(good.clone(), "aud"));
            headers.push(("Subject-Token-Claim-aud".to_owned(), audiences));
            validator.set_answer(ValidatorAnswer::Headers {
                status: 200,
                headers,
            });
            assert!(entra.verify(&token).await.is_ok());
        }

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
        assert_eq!(validator.requests().len(), asked);
    }

    #[tokio::test]
    async fn header_claims_meet_the_same_policy_as_built_in_ones() {
        use crate::identity::{AuthenticationError, Authenticator};

        let openid = StubOpenId::start().await;
        let validator = StubValidator::start().await;
        let mut settings = validator.settings_for_headers(&openid);
        settings.required_role = Some("Push.User".to_owned());
        settings.client_apps = vec![Uuid::parse_str(CLIENT_APP).unwrap()];
        let entra = EntraSignIn::new(settings.clone()).unwrap();
        let built_in = EntraSignIn::with_timing(
            EntraSettings {
                validator: None,
                ..settings
            },
            quick(),
        )
        .unwrap();
        let good = changed(
            openid.user_claims(TENANT, USER),
            "roles",
            json!(["Push.User"]),
        );
        let cases = [
            (good.clone(), None),
            (
                changed(good.clone(), "roles", json!(["Other"])),
                Some(Reason::Role),
            ),
            (without(good.clone(), "roles"), Some(Reason::Role)),
            (
                changed(
                    good.clone(),
                    "azp",
                    json!("66666666-0000-4000-8000-000000000006"),
                ),
                Some(Reason::ClientApp),
            ),
            (
                changed(good.clone(), "scp", json!("other")),
                Some(Reason::Scope),
            ),
            (
                changed(without(good.clone(), "scp"), "idtyp", json!("app")),
                Some(Reason::AppOnly),
            ),
        ];
        // Through the authenticators, which log nothing: an app-only `auth.entra.rejected`
        // event in validator mode would confuse `built_in_refusals_carry_no_validator_flag`.
        for (claims, reason) in cases {
            let token = sign(KEY_A, &claims);
            let checked = Authenticator::authenticate(&entra, &token)
                .await
                .map(|_| ());
            let expected = Authenticator::authenticate(&built_in, &token)
                .await
                .map(|_| ());
            assert_eq!(checked, expected, "{claims}");
            let reason = reason.map(|reason| AuthenticationError::Rejected(reason.as_str()));
            assert_eq!(checked, reason.map_or(Ok(()), Err), "{claims}");
        }
    }

    #[tokio::test]
    async fn a_401_or_403_from_a_header_validator_is_a_validator_refusal() {
        let (openid, validator, entra) = headers_mode().await;
        let challenge = (
            "WWW-Authenticate".to_owned(),
            r#"Bearer error="invalid_token", error_description="secret-desc""#.to_owned(),
        );
        for status in [401, 403] {
            validator.set_answer(ValidatorAnswer::Headers {
                status,
                headers: vec![challenge.clone()],
            });
            assert_eq!(
                entra.verify(&openid.token(USER)).await,
                Err(EntraError::Rejected(Reason::Validator)),
                "{status}"
            );
        }
        // Its body is drained and ignored, whatever its size; claim headers do not count.
        validator.set_answer(ValidatorAnswer::Status(401, "x".repeat(100 * 1024)));
        assert_eq!(
            entra.verify(&openid.token(USER)).await,
            Err(EntraError::Rejected(Reason::Validator))
        );
        validator.set_answer(ValidatorAnswer::Headers {
            status: 401,
            headers: claim_headers(&openid.user_claims(TENANT, USER)),
        });
        assert_eq!(
            entra.verify(&openid.token(USER)).await,
            Err(EntraError::Rejected(Reason::Validator))
        );
    }

    #[test]
    fn the_error_of_a_challenge_is_read_without_its_description() {
        for (challenge, error) in [
            (
                r#"Bearer error="invalid_token", error_description="d""#,
                Some("invalid_token"),
            ),
            (
                r#"Bearer realm="x", error=insufficient_scope"#,
                Some("insufficient_scope"),
            ),
            (
                r#"Bearer error_description="error=fake", error="e""#,
                Some("e"),
            ),
            (r#"Basic realm="a, b", Bearer ERROR="e2""#, Some("e2")),
            (r#"Bearer realm="error=x""#, None),
            ("Bearer", None),
            ("Negotiate abc==", None),
            (r#"Bearer error="bad value""#, None),
            (r#"Bearer error="unterminated"#, None),
        ] {
            assert_eq!(
                validator::challenge_error(challenge).as_deref(),
                error,
                "{challenge}"
            );
        }
        let long = format!("Bearer error=\"{}\"", "e".repeat(100));
        assert_eq!(validator::challenge_error(&long), Some("e".repeat(64)));
    }

    #[tokio::test]
    async fn an_unusable_header_validator_fails_closed_without_falling_back_to_keys() {
        use crate::identity::{AuthenticationError, Authenticator};

        let (openid, validator, entra) = headers_mode().await;
        let token = openid.token(USER);
        let good = claim_headers(&openid.user_claims(TENANT, USER));
        let status = |status: u16| ValidatorAnswer::Headers {
            status,
            headers: good.clone(),
        };
        let mut many = good.clone();
        many.extend((0..65).map(|index| (format!("Subject-Token-Claim-x{index}"), "v".to_owned())));
        let mut large = good.clone();
        large.push(("Subject-Token-Claim-big".to_owned(), "v".repeat(9 * 1024)));
        let mut total = good.clone();
        total.extend((0..5).map(|index| {
            (
                format!("Subject-Token-Claim-big{index}"),
                "v".repeat(7 * 1024),
            )
        }));
        let mut redirect = good.clone();
        redirect.push(("Location".to_owned(), "http://127.0.0.1:1/".to_owned()));
        let answers = [
            status(400),
            status(404),
            status(429),
            status(500),
            status(503),
            ValidatorAnswer::Headers {
                status: 302,
                headers: redirect,
            },
            ValidatorAnswer::Headers {
                status: 200,
                headers: many,
            },
            ValidatorAnswer::Headers {
                status: 200,
                headers: large,
            },
            ValidatorAnswer::Headers {
                status: 200,
                headers: total,
            },
            ValidatorAnswer::Delay(Duration::from_millis(1500)),
        ];
        for answer in &answers {
            validator.set_answer(answer.clone());
            let verified = entra.verify(&token).await;
            let Err(EntraError::Unavailable(error)) = verified else {
                panic!("{answer:?}: {verified:?}");
            };
            assert!(!error.contains(&token), "{error}");
        }
        assert_eq!(validator.requests().len(), answers.len(), "no retries");
        validator.set_answer(status(500));
        assert!(matches!(
            Authenticator::authenticate(&entra, &token).await,
            Err(AuthenticationError::Unavailable(_))
        ));
        let mut settings = validator.settings_for_headers(&openid);
        settings.validator.as_mut().unwrap().url = refused_url().await;
        let refused = EntraSignIn::new(settings).unwrap();
        assert!(matches!(
            refused.verify(&token).await,
            Err(EntraError::Unavailable(_))
        ));
        entra.warm().await;
        refused.warm().await;
        assert_eq!((openid.metadata_fetches(), openid.jwks_fetches()), (0, 0));
    }

    #[tokio::test]
    async fn a_header_validator_without_request_headers_is_warned_about() {
        crate::tests::capture_logs();
        let openid = StubOpenId::start().await;
        let validator = StubValidator::start().await;
        let mut settings = validator.settings_for_headers(&openid);
        settings.validator.as_mut().unwrap().request_headers.clear();
        EntraSignIn::new(settings).unwrap();
        assert!(
            crate::tests::captured_logs().contains("no PUSH_ENTRA_VALIDATOR_REQUEST_HEADERS"),
            "no start-up warning"
        );
    }
}
