//! The `agent-key` provider against a real PostgreSQL: the authenticator and its durable
//! replay cache.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use super::agents::{call, caller, principal, setup};
use super::{Offer, World, password_user, world};
use crate::agent_key::testing::TestKey;
use crate::api::{Api, router};
use crate::identity::{AuthenticationError, Authenticator};
use crate::limits::{Limits, MemoryCounter};
use crate::service::unix_now;
use crate::{AgentKeySettings, AgentKeySignIn, Auth, AuthError, NoDirectory, Tokens};

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

/// `body` posted to the agent-key route.
async fn post_key(router: &Router, body: Value) -> (StatusCode, Value) {
    call(router, "POST", "/v1/auth/agent-key", "", body).await
}

/// Redeems `code` with a fresh assertion of `key` over HTTP.
async fn enroll(router: &Router, key: &TestKey, code: &str) -> (StatusCode, Value) {
    post_key(
        router,
        json!({"token": key.assertion(KEY_AUDIENCE), "enrollment_code": code}),
    )
    .await
}

/// Asserts a 401 `token_invalid` answer.
fn assert_token_invalid((status, body): (StatusCode, Value), what: &str) {
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{what}: {body}");
    assert_eq!(body["error"]["code"], "token_invalid", "{what}");
}

/// A key world with a password owner, and an agent `handle` of it enrolled with `key`:
/// the owner's tokens and the agent's device tokens.
async fn enrolled(
    world: &World,
    auth: &Auth,
    router: &Router,
    key: &TestKey,
    handle: &str,
) -> (Tokens, Tokens) {
    let owner = password_user(auth, &world.pool, "owner", 1).await.remove(0);
    let reservation = auth
        .reserve_agent(caller(&owner), handle, "Key agent")
        .await
        .unwrap();
    let (status, body) = enroll(router, key, &reservation.enrollment_code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let agent = auth
        .register_device(
            body["registration_token"].as_str().unwrap(),
            Uuid::now_v7(),
            "Worker",
            "other",
        )
        .await
        .unwrap();
    (owner, agent)
}

#[tokio::test]
async fn a_key_enrolls_signs_in_and_another_key_is_refused() {
    let (world, auth, router) = key_world().await;
    let owner = password_user(&auth, &world.pool, "owner", 1)
        .await
        .remove(0);
    let reservation = auth
        .reserve_agent(caller(&owner), "key_agent", "Key agent")
        .await
        .unwrap();
    let key = TestKey::generate();
    let (status, enrolled) = enroll(&router, &key, &reservation.enrollment_code).await;
    assert_eq!(status, StatusCode::OK, "{enrolled}");
    assert_eq!(enrolled["user_id"], reservation.user_id.to_string());
    let agents = auth.owned_agents(caller(&owner)).await.unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["state"], "active");
    assert_eq!(agents[0]["credential"], "agent-key");
    assert_eq!(agents[0]["key_thumbprint"], key.thumbprint());
    assert_eq!(agents[0]["client_app_id"], Value::Null);

    let (status, signed_in) =
        post_key(&router, json!({"token": key.assertion(KEY_AUDIENCE)})).await;
    assert_eq!(status, StatusCode::OK, "{signed_in}");
    assert_eq!(signed_in["user_id"], reservation.user_id.to_string());
    let device = auth
        .register_device(
            signed_in["registration_token"].as_str().unwrap(),
            Uuid::now_v7(),
            "Worker",
            "other",
        )
        .await
        .unwrap();
    assert_eq!(device.user_id, reservation.user_id);
    let (status, tokens) = post_key(
        &router,
        json!({"token": key.assertion(KEY_AUDIENCE), "device_id": device.device_id}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tokens}");
    assert_eq!(tokens["user_id"], reservation.user_id.to_string());
    assert!(
        tokens["access_token"]
            .as_str()
            .is_some_and(|t| !t.is_empty())
    );

    let accounts = world.accounts().await;
    let other = TestKey::generate();
    assert_token_invalid(
        post_key(&router, json!({"token": other.assertion(KEY_AUDIENCE)})).await,
        "another key",
    );
    assert_token_invalid(
        post_key(
            &router,
            json!({"token": other.assertion(KEY_AUDIENCE), "device_id": device.device_id}),
        )
        .await,
        "another key with a device",
    );
    let now = unix_now();
    let substituted = other.sign(
        &key.header(),
        &json!({"aud": KEY_AUDIENCE, "iat": now, "exp": now + 120, "jti": "substituted-key-0001"}),
    );
    assert_token_invalid(
        post_key(&router, json!({"token": substituted})).await,
        "the enrolled key's JWK signed by another key",
    );
    assert_eq!(world.accounts().await, accounts, "no account was created");
}

#[tokio::test]
async fn assertion_refusals_answer_401() {
    let (world, auth, router) = key_world().await;
    let key = TestKey::generate();
    enrolled(&world, &auth, &router, &key, "refused_agent").await;
    let now = unix_now();
    let claims = |iat: i64, exp: i64, jti: &str| json!({"aud": KEY_AUDIENCE, "iat": iat, "exp": exp, "jti": jti});
    let header = |alg: &str| json!({"alg": alg, "typ": "push-agent+jwt", "jwk": key.jwk()});
    let mut private = key.jwk();
    private["d"] = json!("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    let refused = [
        (
            "an audience mismatch",
            key.assertion("http://elsewhere.local"),
        ),
        (
            "a lifetime of 301 seconds",
            key.sign(
                &key.header(),
                &claims(now, now + 301, "lifetime-301-seconds"),
            ),
        ),
        (
            "an expired assertion",
            key.sign(
                &key.header(),
                &claims(now - 300, now - 120, "expired-assertion-01"),
            ),
        ),
        (
            "alg none",
            key.sign(
                &header("none"),
                &claims(now, now + 120, "algorithm-none-0001"),
            ),
        ),
        (
            "alg HS256",
            key.sign(
                &header("HS256"),
                &claims(now, now + 120, "algorithm-hs256-001"),
            ),
        ),
        (
            "no jwk",
            key.sign(
                &json!({"alg": "ES256", "typ": "push-agent+jwt"}),
                &claims(now, now + 120, "no-jwk-in-header-01"),
            ),
        ),
        (
            "a jwk with d",
            key.sign(
                &json!({"alg": "ES256", "typ": "push-agent+jwt", "jwk": private}),
                &claims(now, now + 120, "private-jwk-member-1"),
            ),
        ),
    ];
    for (what, token) in refused {
        assert_token_invalid(post_key(&router, json!({"token": token})).await, what);
    }
    let token = key.assertion(KEY_AUDIENCE);
    let (status, body) = post_key(&router, json!({"token": token})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_token_invalid(
        post_key(&router, json!({"token": token})).await,
        "a reused jti",
    );
}

#[tokio::test]
async fn one_live_agent_per_key_and_codes_are_single_use() {
    let (world, auth, router) = key_world().await;
    let owner = password_user(&auth, &world.pool, "owner", 1)
        .await
        .remove(0);
    let a = auth
        .reserve_agent(caller(&owner), "agent_a", "A")
        .await
        .unwrap();
    let b = auth
        .reserve_agent(caller(&owner), "agent_b", "B")
        .await
        .unwrap();
    let key = TestKey::generate();
    let (status, body) = enroll(&router, &key, &a.enrollment_code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = enroll(&router, &key, &b.enrollment_code).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "identity_in_use");
    let agents = auth.owned_agents(caller(&owner)).await.unwrap();
    let state = |id: Uuid| {
        agents
            .iter()
            .find(|agent| agent["user_id"] == id.to_string())
            .unwrap()["state"]
            .clone()
    };
    assert_eq!(state(b.user_id), "pending", "the code was not spent");
    let (status, body) = enroll(&router, &TestKey::generate(), &b.enrollment_code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user_id"], b.user_id.to_string());

    let expired = auth
        .clone()
        .with_agents_per_user(5)
        .reserve_agent(caller(&owner), "agent_c", "C")
        .await
        .unwrap();
    sqlx::query(
        "UPDATE auth.agent_enrollment SET expires_at = now() - interval '1 second' \
         WHERE user_id = $1",
    )
    .bind(expired.user_id)
    .execute(&world.pool)
    .await
    .unwrap();
    for (what, code) in [
        ("expired", &expired.enrollment_code),
        ("spent", &a.enrollment_code),
    ] {
        let (status, body) = enroll(&router, &TestKey::generate(), code).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {body}");
        assert_eq!(body["error"]["code"], "enrollment_invalid", "{what}");
    }
}

#[tokio::test]
async fn the_route_is_absent_unless_enabled() {
    let world = world(Offer::Password).await;
    let token = TestKey::generate().assertion(KEY_AUDIENCE);
    for path in ["/v1/auth/agent-key", "/v1/auth/no-such-provider"] {
        let (status, _) = call(&world.router, "POST", path, "", json!({"token": token})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn deleting_a_key_agent_releases_its_key() {
    let (world, auth, router) = key_world().await;
    let key = TestKey::generate();
    let (owner, agent) = enrolled(&world, &auth, &router, &key, "deleted_agent").await;
    auth.change_agent(caller(&owner), agent.user_id, None, None, true)
        .await
        .unwrap();
    let accounts = world.accounts().await;
    assert_token_invalid(
        post_key(&router, json!({"token": key.assertion(KEY_AUDIENCE)})).await,
        "a released key",
    );
    assert_eq!(world.accounts().await, accounts, "no account was created");
    let released: (String, String, Uuid) = sqlx::query_as(
        "SELECT provider, issuer, user_id FROM auth.released_identity WHERE subject = $1",
    )
    .bind(key.thumbprint())
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert_eq!(
        released,
        ("agent-key".into(), "agent-key".into(), agent.user_id)
    );
    let reservation = auth
        .reserve_agent(caller(&owner), "renewed_agent", "Renewed")
        .await
        .unwrap();
    let (status, body) = enroll(&router, &key, &reservation.enrollment_code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user_id"], reservation.user_id.to_string());
}

#[tokio::test]
async fn a_key_agent_is_not_a_human_owner() {
    let (world, auth, router) = key_world().await;
    let (_, agent) = enrolled(&world, &auth, &router, &TestKey::generate(), "key_owner").await;
    assert_eq!(
        auth.reserve_agent(caller(&agent), "nested_agent", "Nested")
            .await
            .err(),
        Some(AuthError::EnrollmentNotPermitted)
    );

    // Even a verified account of the default kind is no human once a key is bound to it.
    let legacy = Uuid::now_v7();
    let key = TestKey::generate();
    sqlx::query(
        "INSERT INTO auth.user_account (user_id, handle, display_name, email_verified_at) \
         VALUES ($1, 'legacy_key', 'Legacy key', now())",
    )
    .bind(legacy)
    .execute(&world.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auth.external_identity (identity_id, provider, issuer, subject, user_id) \
         VALUES ($1, 'agent-key', 'agent-key', $2, $3)",
    )
    .bind(Uuid::now_v7())
    .bind(key.thumbprint())
    .bind(legacy)
    .execute(&world.pool)
    .await
    .unwrap();
    let (status, body) = post_key(&router, json!({"token": key.assertion(KEY_AUDIENCE)})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let device = auth
        .register_device(
            body["registration_token"].as_str().unwrap(),
            Uuid::now_v7(),
            "Legacy",
            "other",
        )
        .await
        .unwrap();
    assert_eq!(
        auth.reserve_agent(caller(&device), "nested_legacy", "Nested")
            .await
            .err(),
        Some(AuthError::EnrollmentNotPermitted)
    );
}

#[tokio::test]
async fn reservations_without_entra_app_have_no_tenant() {
    let (world, auth, _router) = key_world().await;
    let owner = password_user(&auth, &world.pool, "owner", 1)
        .await
        .remove(0);
    let reservation = auth
        .reserve_agent(caller(&owner), "tenantless", "Tenantless")
        .await
        .unwrap();
    let tenant: Option<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM auth.agent_enrollment WHERE user_id = $1")
            .bind(reservation.user_id)
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert_eq!(tenant, None);

    let (world, app, _, owner) = setup().await;
    let both = app.with_agent_key(
        AgentKeySignIn::new(
            settings(KEY_AUDIENCE, Duration::from_secs(3600)),
            world.pool.clone(),
        )
        .unwrap(),
    );
    let reservation = both
        .reserve_agent(caller(&owner), "tenanted", "Tenanted")
        .await
        .unwrap();
    let tenant: Option<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM auth.agent_enrollment WHERE user_id = $1")
            .bind(reservation.user_id)
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert_eq!(tenant, Some(crate::entra::testing::TENANT.parse().unwrap()));
    sqlx::query("UPDATE auth.agent_enrollment SET tenant_id = NULL WHERE user_id = $1")
        .bind(reservation.user_id)
        .execute(&world.pool)
        .await
        .unwrap();
    assert_eq!(
        both.enroll_agent(&principal(Uuid::new_v4()), &reservation.enrollment_code)
            .await
            .err(),
        Some(AuthError::EnrollmentNotPermitted)
    );
    let key = both
        .authenticator("agent-key")
        .unwrap()
        .authenticate(&TestKey::generate().assertion(KEY_AUDIENCE))
        .await
        .unwrap();
    let enrolled = both
        .enroll_agent(&key, &reservation.enrollment_code)
        .await
        .unwrap();
    assert_eq!(enrolled.user_id, reservation.user_id);
}
