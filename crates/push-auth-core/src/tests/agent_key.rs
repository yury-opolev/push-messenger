//! The `agent-key` provider against a real PostgreSQL: the authenticator and its durable
//! replay cache.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use serde_json::json;

use super::{Offer, World, world};
use crate::agent_key::testing::TestKey;
use crate::api::{Api, router};
use crate::identity::{AuthenticationError, Authenticator};
use crate::limits::{Limits, MemoryCounter};
use crate::service::unix_now;
use crate::{AgentKeySettings, AgentKeySignIn, Auth, NoDirectory};

pub(super) const KEY_AUDIENCE: &str = "http://push.local";

fn settings(audience: &str, max_session: Duration) -> AgentKeySettings {
    AgentKeySettings {
        audience: audience.to_owned(),
        max_session,
    }
}

/// A world offering password sign-in and agent-key only (no entra-app), with its router.
pub(super) async fn key_world() -> (World, Auth, Router) {
    let world = world(Offer::Password).await;
    let auth = world.auth.clone().with_agent_key(
        AgentKeySignIn::new(
            settings(KEY_AUDIENCE, Duration::from_secs(3600)),
            world.pool.clone(),
        )
        .unwrap(),
    );
    let router = router(Api {
        auth: auth.clone(),
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: None,
        directory: Arc::new(NoDirectory),
    });
    (world, auth, router)
}

/// An assertion for [`KEY_AUDIENCE`] by `key` with the given `jti`.
fn assertion_with_jti(key: &TestKey, jti: &str) -> String {
    let now = unix_now();
    key.sign(
        &key.header(),
        &json!({"aud": KEY_AUDIENCE, "iat": now, "exp": now + 120, "jti": jti}),
    )
}

#[tokio::test]
async fn a_jti_is_single_use_per_key() {
    let (world, auth, _router) = key_world().await;
    let authenticator = auth.authenticator("agent-key").unwrap();
    assert_eq!(authenticator.max_session(), Duration::from_secs(3600));
    let key = TestKey::generate();

    let token = key.assertion(KEY_AUDIENCE);
    let principal = authenticator.authenticate(&token).await.unwrap();
    assert_eq!(principal.provider, "agent-key");
    assert_eq!(principal.issuer, "agent-key");
    assert_eq!(principal.subject, key.thumbprint());
    assert_eq!(principal.claims["aud"], KEY_AUDIENCE);
    assert!(matches!(
        authenticator.authenticate(&token).await,
        Err(AuthenticationError::Rejected("replay"))
    ));

    let jti = "0123456789abcdef-shared";
    let other = TestKey::generate();
    authenticator
        .authenticate(&assertion_with_jti(&key, jti))
        .await
        .unwrap();
    let principal = authenticator
        .authenticate(&assertion_with_jti(&other, jti))
        .await
        .unwrap();
    assert_eq!(principal.subject, other.thumbprint());
    assert!(matches!(
        authenticator
            .authenticate(&assertion_with_jti(&other, jti))
            .await,
        Err(AuthenticationError::Rejected("replay"))
    ));

    // The cache keeps a use until its expiry plus the clock skew.
    let (expires_at,): (i64,) = sqlx::query_as(
        "SELECT extract(epoch FROM expires_at)::bigint FROM auth.agent_key_assertion \
         WHERE thumbprint = $1 AND jti = $2",
    )
    .bind(other.thumbprint())
    .bind(jti)
    .fetch_one(&world.pool)
    .await
    .unwrap();
    let expected = unix_now() + 120 + 60;
    assert!((expected - 5..=expected).contains(&expires_at));

    // Purged five minutes after expiry, so a clock drift cannot reopen a replay.
    let expired = "fedcba9876543210-expired";
    let recent = "fedcba9876543210-recently";
    sqlx::query(
        "INSERT INTO auth.agent_key_assertion (thumbprint, jti, expires_at) \
         VALUES ($1, $2, now() - interval '301 seconds'), ($1, $3, now() - interval '1 second')",
    )
    .bind(key.thumbprint())
    .bind(expired)
    .bind(recent)
    .execute(&world.pool)
    .await
    .unwrap();
    authenticator
        .authenticate(&key.assertion(KEY_AUDIENCE))
        .await
        .unwrap();
    let (left,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM auth.agent_key_assertion WHERE jti = $1")
            .bind(expired)
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert_eq!(left, 0);
    let (kept,): (i64,) = sqlx::query_as("SELECT count(*) FROM auth.agent_key_assertion")
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(kept, 5, "unexpired and recently expired uses stay");
}

#[tokio::test]
async fn concurrent_uses_of_one_assertion_admit_one() {
    let (_world, auth, _router) = key_world().await;
    let authenticator = auth.authenticator("agent-key").unwrap();
    let token = TestKey::generate().assertion(KEY_AUDIENCE);
    let (first, second) = tokio::join!(
        authenticator.authenticate(&token),
        authenticator.authenticate(&token)
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
}

#[tokio::test]
async fn an_unreachable_replay_cache_fails_closed() {
    let world = world(Offer::Password).await;
    let authenticator = AgentKeySignIn::new(
        settings(KEY_AUDIENCE, Duration::from_secs(3600)),
        world.pool.clone(),
    )
    .unwrap();
    world.pool.close().await;
    let token = TestKey::generate().assertion(KEY_AUDIENCE);
    assert!(matches!(
        authenticator.authenticate(&token).await,
        Err(AuthenticationError::Unavailable(_))
    ));
}

#[tokio::test]
async fn agent_key_settings_are_checked() {
    let world = world(Offer::Password).await;
    let hour = Duration::from_secs(3600);
    for audience in ["", "a b", &"a".repeat(257)] {
        assert!(
            AgentKeySignIn::new(settings(audience, hour), world.pool.clone()).is_err(),
            "audience {audience:?}"
        );
    }
    for session in [Duration::from_secs(59 * 60), 721 * hour] {
        assert!(
            AgentKeySignIn::new(settings(KEY_AUDIENCE, session), world.pool.clone()).is_err(),
            "session {session:?}"
        );
    }
    for session in [hour, 720 * hour] {
        AgentKeySignIn::new(settings(&"a".repeat(256), session), world.pool.clone()).unwrap();
    }
}
