//! The opt-in `agent-key` provider (spec 2026-10-26 agent-key provider): an owned agent
//! proves possession of the P-256 key it bound at enrollment with a single-use ES256
//! assertion. Push-messenger keeps only the key's thumbprint.

use std::time::Duration;

use sqlx::PgPool;

use crate::config::ConfigError;
use crate::identity::{AGENT_KEY_PROVIDER, AuthenticationError, Authenticator, Principal};

mod jws;

const MAX_SESSION_HOURS: u64 = 720;

/// Policy of the explicitly enabled `agent-key` provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentKeySettings {
    /// The `aud` every assertion must carry, e.g. the public API URL.
    pub audience: String,
    /// How long a device signed in with a key stays signed in: 1 to 720 hours.
    pub max_session: Duration,
}

impl AgentKeySettings {
    /// `PUSH_AUTH_AGENT_KEY_AUDIENCE` (trimmed; default `public_url` without a trailing
    /// `/`) and `PUSH_AUTH_AGENT_KEY_MAX_SESSION_HOURS` (a whole number from 1 to 720;
    /// default 24), read through `lookup`.
    pub fn from_lookup(
        lookup: &dyn Fn(&str) -> Option<String>,
        public_url: &str,
    ) -> Result<Self, ConfigError> {
        const AUDIENCE: &str = "PUSH_AUTH_AGENT_KEY_AUDIENCE";
        const HOURS: &str = "PUSH_AUTH_AGENT_KEY_MAX_SESSION_HOURS";
        let audience = lookup(AUDIENCE)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| public_url.trim_end_matches('/').to_owned());
        if !is_audience(&audience) {
            return Err(ConfigError::Invalid {
                name: AUDIENCE,
                value: audience,
                reason: "use 1 to 256 visible ASCII characters".to_owned(),
            });
        }
        let hours_value = lookup(HOURS).unwrap_or_else(|| "24".to_owned());
        let hours: u64 = hours_value
            .trim()
            .parse()
            .ok()
            .filter(|hours| (1..=MAX_SESSION_HOURS).contains(hours))
            .ok_or_else(|| ConfigError::Invalid {
                name: HOURS,
                value: hours_value.clone(),
                reason: "use a whole number of hours from 1 to 720".to_owned(),
            })?;
        Ok(Self {
            audience,
            max_session: Duration::from_secs(hours * 3600),
        })
    }
}

/// Refuses an audience that is not 1 to 256 visible ASCII characters and a session
/// outside 1 to 720 hours: what [`AgentKeySignIn::new`] checks, for a start-up that
/// validates before it has a pool.
pub fn check(settings: &AgentKeySettings) -> Result<(), String> {
    if !is_audience(&settings.audience) {
        return Err("the agent-key audience must be 1 to 256 visible ASCII characters".to_owned());
    }
    if settings.max_session < Duration::from_secs(3600)
        || settings.max_session > Duration::from_secs(MAX_SESSION_HOURS * 3600)
    {
        return Err("the agent-key maximum session must be 1 to 720 hours".to_owned());
    }
    Ok(())
}

/// The `agent-key` authenticator; its replay cache is the `auth` database.
#[derive(Debug, Clone)]
pub struct AgentKeySignIn {
    settings: AgentKeySettings,
    pool: PgPool,
}

impl AgentKeySignIn {
    /// Refuses an audience that is not 1 to 256 visible ASCII characters and a session
    /// outside 1 to 720 hours.
    pub fn new(settings: AgentKeySettings, pool: PgPool) -> Result<Self, String> {
        check(&settings)?;
        Ok(Self { settings, pool })
    }

    /// Records the assertion's first use; `false` for a replay.
    async fn first_use(&self, assertion: &jws::Assertion) -> Result<bool, sqlx::Error> {
        // Rows are kept five minutes past their expiry, so a database clock ahead of a
        // replica's cannot purge an entry that replica would still accept. A bounded
        // batch that skips rows other sign-ins hold never waits on them nor deadlocks.
        sqlx::query(
            "DELETE FROM auth.agent_key_assertion WHERE ctid IN ( \
             SELECT ctid FROM auth.agent_key_assertion \
             WHERE expires_at < now() - interval '5 minutes' \
             LIMIT 100 FOR UPDATE SKIP LOCKED)",
        )
        .execute(&self.pool)
        .await?;
        let inserted = sqlx::query(
            "INSERT INTO auth.agent_key_assertion (thumbprint, jti, expires_at) \
             VALUES ($1, $2, to_timestamp($3::bigint)) ON CONFLICT DO NOTHING RETURNING 1",
        )
        .bind(&assertion.thumbprint)
        .bind(&assertion.jti)
        .bind(assertion.exp + jws::CLOCK_SKEW_SECONDS)
        .fetch_optional(&self.pool)
        .await?;
        Ok(inserted.is_some())
    }
}

/// Whether `value` is an acceptable audience: 1 to 256 visible ASCII characters.
pub(crate) fn is_audience(value: &str) -> bool {
    (1..=256).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_graphic())
}

#[async_trait::async_trait]
impl Authenticator for AgentKeySignIn {
    fn provider(&self) -> &str {
        AGENT_KEY_PROVIDER
    }

    fn max_session(&self) -> Duration {
        self.settings.max_session
    }

    async fn authenticate(&self, token: &str) -> Result<Principal, AuthenticationError> {
        let assertion = jws::verify(token, &self.settings.audience, crate::service::unix_now())
            .map_err(AuthenticationError::Rejected)?;
        match self.first_use(&assertion).await {
            Ok(true) => {}
            Ok(false) => return Err(AuthenticationError::Rejected("replay")),
            Err(error) => {
                tracing::warn!(%error, "an agent-key sign-in failed: the replay cache is unreachable");
                return Err(AuthenticationError::Unavailable(
                    "the agent-key replay cache is unreachable".to_owned(),
                ));
            }
        }
        Ok(Principal {
            provider: AGENT_KEY_PROVIDER.to_owned(),
            issuer: AGENT_KEY_PROVIDER.to_owned(),
            subject: assertion.thumbprint,
            name: None,
            email: None,
            claims: assertion.claims,
        })
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A P-256 signing key for tests and the assertions it makes.

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    use serde_json::{Value, json};

    pub(crate) struct TestKey {
        pair: EcdsaKeyPair,
    }

    impl TestKey {
        pub(crate) fn generate() -> Self {
            let rng = SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            let pair =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap();
            Self { pair }
        }

        /// The public JWK `{kty, crv, x, y}`.
        pub(crate) fn jwk(&self) -> Value {
            let point = self.pair.public_key().as_ref();
            json!({"kty": "EC", "crv": "P-256",
                "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
                "y": URL_SAFE_NO_PAD.encode(&point[33..65])})
        }

        pub(crate) fn thumbprint(&self) -> String {
            let point = self.pair.public_key().as_ref();
            super::jws::thumbprint(&point[1..33], &point[33..65])
        }

        /// A valid assertion for `audience`, issued now, valid for two minutes.
        pub(crate) fn assertion(&self, audience: &str) -> String {
            let now = crate::service::unix_now();
            self.sign(
                &self.header(),
                &json!({"aud": audience, "iat": now, "exp": now + 120,
                    "jti": uuid::Uuid::new_v4().simple().to_string()}),
            )
        }

        /// The standard header: ES256, push-agent+jwt and this key's JWK.
        pub(crate) fn header(&self) -> Value {
            json!({"alg": "ES256", "typ": "push-agent+jwt", "jwk": self.jwk()})
        }

        /// `header` and `claims` as JSON, signed with this key.
        pub(crate) fn sign(&self, header: &Value, claims: &Value) -> String {
            self.sign_raw(&header.to_string(), &claims.to_string())
        }

        /// Raw header and claims texts (for duplicate members), signed with this key.
        pub(crate) fn sign_raw(&self, header: &str, claims: &str) -> String {
            let signed = format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(header),
                URL_SAFE_NO_PAD.encode(claims)
            );
            let signature = self
                .pair
                .sign(&SystemRandom::new(), signed.as_bytes())
                .unwrap();
            format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
        }
    }
}
