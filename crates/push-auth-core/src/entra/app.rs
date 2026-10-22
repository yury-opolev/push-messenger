//! App-only policy, independent of delegated-user scopes, roles and client filters.

use std::time::Duration;

use uuid::Uuid;

use super::{EntraError, EntraSignIn, Reason, Tenants};
use crate::config::ConfigError;
use crate::identity::{AuthenticationError, Authenticator, ENTRA_APP_PROVIDER, Principal};

/// Policy of the explicitly enabled `entra-app` provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntraAppSettings {
    pub required_role: Option<String>,
    pub client_apps: Vec<Uuid>,
    pub max_session: Duration,
}

impl Default for EntraAppSettings {
    fn default() -> Self {
        Self {
            required_role: None,
            client_apps: Vec::new(),
            max_session: Duration::from_secs(24 * 3600),
        }
    }
}

impl EntraAppSettings {
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let (required_role, client_apps, max_session) = super::settings::policy(
            lookup,
            [
                "PUSH_ENTRA_APP_REQUIRED_ROLE",
                "PUSH_ENTRA_APP_ALLOWED_CLIENT_APPS",
                "PUSH_ENTRA_APP_MAX_SESSION_HOURS",
            ],
        )?;
        Ok(Self {
            required_role,
            client_apps,
            max_session,
        })
    }
}

/// App-only view of an Entra verifier; clones share its signing-key cache.
#[derive(Debug, Clone)]
pub struct EntraAppSignIn {
    verifier: EntraSignIn,
    settings: EntraAppSettings,
}

impl EntraAppSignIn {
    pub(crate) fn tenants(&self) -> &[Uuid] {
        match &self.verifier.settings.tenants {
            Tenants::Ids(ids) => ids,
            _ => unreachable!("app construction requires explicit tenants"),
        }
    }
}

impl EntraSignIn {
    /// Creates a separately scoped app-only provider sharing this verifier's key cache.
    /// Unrestricted tenant keywords and sessions outside 1 to 720 hours are refused.
    pub fn app(&self, settings: EntraAppSettings) -> Result<EntraAppSignIn, String> {
        if !matches!(&self.settings.tenants, Tenants::Ids(ids) if !ids.is_empty() && ids.iter().all(|id| !id.is_nil()))
        {
            return Err("entra-app requires explicit tenant GUIDs".to_owned());
        }
        if settings.max_session < Duration::from_secs(3600)
            || settings.max_session > Duration::from_secs(720 * 3600)
        {
            return Err("entra-app maximum session must be 1 to 720 hours".to_owned());
        }
        Ok(EntraAppSignIn {
            verifier: self.clone(),
            settings,
        })
    }
}

#[async_trait::async_trait]
impl Authenticator for EntraAppSignIn {
    fn provider(&self) -> &str {
        ENTRA_APP_PROVIDER
    }

    fn max_session(&self) -> Duration {
        self.settings.max_session
    }

    async fn authenticate(&self, token: &str) -> Result<Principal, AuthenticationError> {
        let (claims, tenant, payload) = self.verifier.verified(token).await.map_err(|error| {
            match error {
                EntraError::Rejected(reason) => AuthenticationError::Rejected(reason.as_str()),
                EntraError::Unavailable(error) => {
                    tracing::warn!(%error, "an Entra app sign-in failed: no usable signing keys");
                    AuthenticationError::Unavailable(error)
                }
            }
        })?;
        let reject = |reason: Reason| AuthenticationError::Rejected(reason.as_str());
        super::claims::common(&claims, tenant).map_err(reject)?;
        if claims
            .scp
            .as_deref()
            .is_some_and(|scope| !scope.trim().is_empty())
            || claims.idtyp.as_deref().is_some_and(|kind| kind != "app")
        {
            return Err(reject(Reason::AppOnly));
        }
        let guid = |value: Option<&str>| {
            value
                .and_then(|s| Uuid::try_parse(s).ok())
                .filter(|id| !id.is_nil())
        };
        let object = guid(claims.oid.as_deref()).ok_or_else(|| reject(Reason::Subject))?;
        let client = guid(claims.azp.as_deref()).ok_or_else(|| reject(Reason::ClientApp))?;
        if !self.settings.client_apps.is_empty() && !self.settings.client_apps.contains(&client) {
            return Err(reject(Reason::ClientApp));
        }
        if self
            .settings
            .required_role
            .as_ref()
            .is_some_and(|role| !claims.roles.contains(role))
        {
            return Err(reject(Reason::Role));
        }
        Ok(Principal {
            provider: ENTRA_APP_PROVIDER.to_owned(),
            issuer: tenant.to_string(),
            subject: object.to_string(),
            name: crate::identity::informational(claims.name.as_deref()),
            email: None,
            claims: payload,
        })
    }

    async fn warm(&self) {
        self.verifier.warm().await;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::entra::testing::{
        CLIENT_APP, KEY_A, KEY_B, StubOpenId, TENANT, sign, sign_as, with_algorithm,
    };
    use crate::entra::{EntraSignIn, Tenants};
    use crate::identity::Authenticator;

    const OBJECT: &str = "55555555-0000-4000-8000-000000000005";

    #[tokio::test]
    async fn app_tokens_have_a_distinct_principal_and_share_delegated_keys() {
        let stub = StubOpenId::start().await;
        let mut settings = stub.settings();
        settings.required_role = Some("User.Role".to_owned());
        settings.client_apps = vec![Uuid::new_v4()];
        let delegated = EntraSignIn::new(settings).unwrap();
        let app = delegated.app(EntraAppSettings::default()).unwrap();
        let mut claims = stub.user_claims(TENANT, OBJECT);
        claims.as_object_mut().unwrap().remove("scp");
        claims["idtyp"] = json!("app");
        let principal = app.authenticate(&sign(KEY_A, &claims)).await.unwrap();
        assert_eq!(app.provider(), "entra-app");
        assert_eq!(principal.provider, "entra-app");
        assert_eq!(principal.issuer, TENANT);
        assert_eq!(principal.subject, OBJECT);
        assert_eq!(principal.claims["azp"], CLIENT_APP);
        assert_eq!(app.max_session(), Duration::from_secs(24 * 3600));
        assert!(delegated.authenticate(&sign(KEY_A, &claims)).await.is_err());
        claims.as_object_mut().unwrap().remove("idtyp");
        assert!(app.authenticate(&sign(KEY_A, &claims)).await.is_ok());
        assert_eq!(stub.jwks_fetches(), 1);
        for tenants in [Tenants::Common, Tenants::Organizations, Tenants::Consumers] {
            let mut settings = stub.settings();
            settings.tenants = tenants;
            assert!(
                EntraSignIn::new(settings)
                    .unwrap()
                    .app(EntraAppSettings::default())
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn app_claim_policy_and_crypto_are_fail_closed() {
        let stub = StubOpenId::start().await;
        let delegated = EntraSignIn::new(stub.settings()).unwrap();
        let app = delegated
            .app(EntraAppSettings {
                required_role: Some("Agent.Run".to_owned()),
                client_apps: vec![Uuid::parse_str(CLIENT_APP).unwrap()],
                max_session: Duration::from_secs(3600),
            })
            .unwrap();
        let mut good = stub.user_claims(TENANT, OBJECT);
        good.as_object_mut().unwrap().remove("scp");
        good["idtyp"] = json!("app");
        good["roles"] = json!(["Agent.Run"]);
        assert!(app.authenticate(&sign(KEY_A, &good)).await.is_ok());
        for (key, value) in [
            ("scp", json!("access_as_user")),
            ("scp", json!("other")),
            ("idtyp", json!("user")),
            ("idtyp", json!("unknown")),
            ("oid", json!("not-a-guid")),
            ("oid", json!(Uuid::nil())),
            ("tid", json!(Uuid::nil())),
            ("azp", json!("not-a-guid")),
            ("azp", json!(Uuid::nil())),
            ("azp", json!(Uuid::new_v4())),
            ("roles", json!(["User.Role"])),
            ("ver", json!("1.0")),
            ("iss", json!("https://evil.example/v2.0")),
            ("aud", json!(CLIENT_APP)),
            ("exp", json!(crate::service::unix_now() - 120)),
            ("nbf", json!(crate::service::unix_now() + 120)),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(app.authenticate(&sign(KEY_A, &bad)).await.is_err(), "{key}");
        }
        for missing in ["tid", "oid", "azp", "exp", "nbf", "aud", "iss"] {
            let mut bad = good.clone();
            bad.as_object_mut().unwrap().remove(missing);
            assert!(
                app.authenticate(&sign(KEY_A, &bad)).await.is_err(),
                "{missing}"
            );
        }
        for token in [
            sign_as(KEY_B, KEY_A.kid, &good),
            sign(KEY_B, &good),
            with_algorithm("HS256", KEY_A.kid, &good, "fake"),
            with_algorithm("none", KEY_A.kid, &good, ""),
            "garbage".to_owned(),
        ] {
            assert!(app.authenticate(&token).await.is_err());
        }
        assert!(app.authenticate(&stub.token(OBJECT)).await.is_err());
    }
}
