//! A push-auth of one's own (spec 2026-10-19 pluggable identity, D10): the built-in
//! Microsoft Entra ID check wrapped by an authenticator that refuses guest accounts, and an
//! admission that lets only the members of one Entra group sign in.
//!
//! Run it like push-auth, with `PUSH_AUTH_PROVIDERS=external` (or `password,external`), the
//! usual `PUSH_ENTRA_*` variables and `EXAMPLE_REQUIRED_GROUP`, the object id of the group
//! (the API registration must emit the `groups` claim). The authenticator keeps the
//! provider id `entra`, so users still sign in at `POST /v1/auth/entra` and the SDK's
//! `sign_in_with_entra` works unchanged. See docs/entra-sign-in.md, "Your own wrapper".
//!
//! ```text
//! cargo run -p push-auth --example custom_identity
//! cargo run -p push-auth --example custom_identity -- migrate
//! ```

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use push_auth_core::entra::{EntraSettings, EntraSignIn};
use push_auth_core::{
    Admission, AdmissionDecision, AuthenticationError, Authenticator, Config, MigrateConfig,
    Overrides, Principal, async_trait, init_logging, migrate, serve_with, terminated,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// The built-in Entra ID check, refusing guests: users of other tenants invited into the
/// tenant (`acct` 1, an optional claim of the API registration).
#[derive(Debug)]
struct NoGuests {
    entra: EntraSignIn,
}

#[async_trait]
impl Authenticator for NoGuests {
    /// Entra ID's id: the route stays `/v1/auth/entra`.
    fn provider(&self) -> &str {
        Authenticator::provider(&self.entra)
    }

    fn max_session(&self) -> Duration {
        Authenticator::max_session(&self.entra)
    }

    async fn authenticate(&self, token: &str) -> Result<Principal, AuthenticationError> {
        // Signature, issuer, audience, lifetime, delegated token and the object id are
        // checked by the built-in check; this only adds a rule.
        let principal = self.entra.authenticate(token).await?;
        if principal.claims.get("acct").and_then(|acct| acct.as_i64()) == Some(1) {
            return Err(AuthenticationError::Rejected("guest"));
        }
        Ok(principal)
    }

    async fn warm(&self) {
        Authenticator::warm(&self.entra).await;
    }
}

/// Admits the members of one group (its object id in the `groups` claim).
#[derive(Debug)]
struct GroupMember {
    group: String,
}

#[async_trait]
impl Admission for GroupMember {
    async fn admit(&self, principal: &Principal) -> AdmissionDecision {
        let member = principal
            .claims
            .get("groups")
            .and_then(|groups| groups.as_array())
            .is_some_and(|groups| groups.iter().any(|group| group == self.group.as_str()));
        if member {
            AdmissionDecision::Admit
        } else {
            AdmissionDecision::Refuse
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    if std::env::args().nth(1).as_deref() == Some("migrate") {
        return migrate(MigrateConfig::from_env()?).await;
    }
    let config = Config::from_env()?;
    // PUSH_AUTH_PROVIDERS names external, so Config reads no PUSH_ENTRA_*: the wrapper
    // reads them itself.
    let entra = EntraSignIn::new(EntraSettings::from_env()?).map_err(anyhow::Error::msg)?;
    let group = std::env::var("EXAMPLE_REQUIRED_GROUP")
        .ok()
        .map(|group| group.trim().to_owned())
        .filter(|group| !group.is_empty())
        .context("EXAMPLE_REQUIRED_GROUP: the object id of the group whose members may sign in")?;
    let overrides = Overrides {
        authenticator: Some(Arc::new(NoGuests { entra })),
        admission: Some(Arc::new(GroupMember { group })),
    };
    let http = TcpListener::bind(config.http_addr).await?;
    let health = TcpListener::bind(config.health_addr).await?;
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        terminated().await;
        tracing::info!("shutting down");
        signal.cancel();
    });
    serve_with(config, http, health, shutdown, overrides).await
}

#[cfg(test)]
mod tests {
    use push_auth_core::entra::testing::{KEY_A, StubOpenId, TENANT, sign};
    use serde_json::json;

    use super::*;

    const USER: &str = "55555555-0000-4000-8000-000000000005";
    const STAFF: &str = "66666666-0000-4000-8000-000000000006";

    #[tokio::test]
    async fn members_sign_in_others_are_refused_and_guests_rejected() {
        let stub = StubOpenId::start().await;
        let wrapper = NoGuests {
            entra: EntraSignIn::new(stub.settings()).unwrap(),
        };
        let admission = GroupMember {
            group: STAFF.to_owned(),
        };
        assert_eq!(wrapper.provider(), "entra");
        assert_eq!(wrapper.max_session(), Duration::from_secs(24 * 3600));

        let mut claims = stub.user_claims(TENANT, USER);
        claims["groups"] = json!([STAFF]);
        let member = wrapper.authenticate(&sign(KEY_A, &claims)).await.unwrap();
        assert_eq!(member.subject, USER);
        assert_eq!(admission.admit(&member).await, AdmissionDecision::Admit);

        claims["groups"] = json!(["77777777-0000-4000-8000-000000000007"]);
        let outsider = wrapper.authenticate(&sign(KEY_A, &claims)).await.unwrap();
        assert_eq!(admission.admit(&outsider).await, AdmissionDecision::Refuse);
        claims.as_object_mut().unwrap().remove("groups");
        let no_groups = wrapper.authenticate(&sign(KEY_A, &claims)).await.unwrap();
        assert_eq!(admission.admit(&no_groups).await, AdmissionDecision::Refuse);

        claims["acct"] = json!(1);
        assert_eq!(
            wrapper.authenticate(&sign(KEY_A, &claims)).await,
            Err(AuthenticationError::Rejected("guest"))
        );
        // The built-in rules still hold.
        claims["acct"] = json!(0);
        claims["aud"] = json!("api://someone-else");
        assert_eq!(
            wrapper.authenticate(&sign(KEY_A, &claims)).await,
            Err(AuthenticationError::Rejected("audience"))
        );
    }
}
