use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use super::{Offer, World, password_user, world};
use crate::api::{Api, router};
use crate::entra::EntraAppSettings;
use crate::entra::testing::{CLIENT_APP, KEY_A, TENANT, sign};
use crate::limits::{Limits, MemoryCounter};
use crate::{Auth, NoDirectory, Tokens};
use crate::{AuthError, Caller, Principal, Signup};

pub(super) fn caller(tokens: &Tokens) -> Caller {
    Caller {
        user_id: tokens.user_id,
        device_id: tokens.device_id,
    }
}

pub(super) fn principal(object: Uuid) -> Principal {
    Principal {
        provider: "entra-app".into(),
        issuer: TENANT.into(),
        subject: object.to_string(),
        name: None,
        email: None,
        claims: serde_json::from_value(json!({"azp":CLIENT_APP})).unwrap(),
    }
}

async fn wait_for_account_waiters(pool: &sqlx::PgPool, count: i64) {
    use std::time::Duration;

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock' \
                 AND query LIKE '%auth.user_account%'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting >= count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("requests reach the account lock");
}

#[tokio::test]
async fn profile_patch_losing_to_agent_deletion_preserves_the_tombstone() {
    let (world, auth, routes, owner) = setup().await;
    let reservation = auth
        .reserve_agent(caller(&owner), "profile_agent", "Original")
        .await
        .unwrap();
    let registration = auth
        .enroll_agent(&principal(Uuid::new_v4()), &reservation.enrollment_code)
        .await
        .unwrap();
    let agent = auth
        .register_device(
            &registration.registration.token,
            Uuid::now_v7(),
            "Worker",
            "other",
        )
        .await
        .unwrap();
    for tokens in [&owner, &agent] {
        let (status, body) = call(
            &routes,
            "PATCH",
            "/v1/users/me",
            &tokens.access_token,
            json!({"display_name":"Enabled"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["display_name"], "Enabled");
    }

    let mut holder = world.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM auth.user_account WHERE user_id = $1 FOR UPDATE")
        .bind(agent.user_id)
        .execute(&mut *holder)
        .await
        .unwrap();
    let deleting = auth.clone();
    let owner_caller = caller(&owner);
    let agent_id = agent.user_id;
    let deletion = tokio::spawn(async move {
        deleting
            .change_agent(owner_caller, agent_id, None, None, true)
            .await
    });
    wait_for_account_waiters(&world.pool, 1).await;
    let patch = tokio::spawn(async move {
        call(
            &routes,
            "PATCH",
            "/v1/users/me",
            &agent.access_token,
            json!({"handle":"stolen_tombstone","display_name":"Changed after deletion"}),
        )
        .await
    });
    wait_for_account_waiters(&world.pool, 2).await;
    holder.commit().await.unwrap();
    let (deleted, patched) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(deletion, patch)
    })
    .await
    .expect("deletion and profile editing finish without a lock cycle");
    deleted.unwrap().unwrap();
    let (status, body) = patched.unwrap();
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let tombstone: (String, String, bool) = sqlx::query_as(
        "SELECT handle::text, display_name, disabled_at IS NOT NULL \
         FROM auth.user_account WHERE user_id = $1",
    )
    .bind(agent_id)
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert_eq!(tombstone, ("profile_agent".into(), "Enabled".into(), true));
    assert_eq!(
        auth.reserve_agent(owner_caller, "profile_agent", "Replacement")
            .await
            .err(),
        Some(AuthError::HandleTaken)
    );
}

#[tokio::test]
async fn profile_edit_and_agent_management_do_not_invert_expired_reservation_locks() {
    use std::time::Duration;

    let (world, auth, _, owner) = setup().await;
    let reserved = auth
        .reserve_agent(caller(&owner), "expired_agent", "Expired")
        .await
        .unwrap();
    sqlx::query("UPDATE auth.agent_enrollment SET expires_at = now() - interval '1 second'")
        .execute(&world.pool)
        .await
        .unwrap();

    let mut holder = world.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM auth.user_account WHERE user_id = $1 FOR UPDATE")
        .bind(owner.user_id)
        .execute(&mut *holder)
        .await
        .unwrap();
    // Queue management first so it takes the owner lock before the profile edit.
    let managing = auth.clone();
    let owner_caller = caller(&owner);
    let management = tokio::spawn(async move { managing.owned_agents(owner_caller).await });
    wait_for_account_waiters(&world.pool, 1).await;
    let editing = auth.clone();
    let profile = tokio::spawn(async move {
        editing
            .update_profile(owner_caller, Some("Updated"), None)
            .await
    });
    wait_for_account_waiters(&world.pool, 2).await;
    holder.commit().await.unwrap();

    let (management, profile) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(management, profile)
    })
    .await
    .expect("management and profile editing must finish without a lock cycle");
    assert!(management.unwrap().unwrap().is_empty());
    assert_eq!(profile.unwrap().unwrap().display_name, "Updated");
    assert!(auth.profiles(&[reserved.user_id]).await.unwrap().is_empty());
    assert!(
        auth.reserve_agent(owner_caller, "expired_agent", "Reclaimed")
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn expired_handles_remain_reclaimable_across_owners_and_account_paths() {
    let (world, auth, _, owner) = setup().await;
    let other = password_user(&auth, &world.pool, "other_owner", 1)
        .await
        .remove(0);
    let target = auth
        .reserve_agent(caller(&other), "rename_target", "Target")
        .await
        .unwrap();
    for handle in [
        "reclaim_profile",
        "reclaim_agent",
        "reclaim_reservation",
        "reclaim_external",
    ] {
        let expired = auth
            .reserve_agent(caller(&owner), handle, "Expired")
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
        match handle {
            "reclaim_profile" => {
                auth.update_profile(caller(&other), None, Some(handle))
                    .await
                    .unwrap();
            }
            "reclaim_agent" => {
                auth.change_agent(caller(&other), target.user_id, Some(handle), None, false)
                    .await
                    .unwrap();
            }
            "reclaim_reservation" => {
                auth.reserve_agent(caller(&other), handle, "Reclaimed")
                    .await
                    .unwrap();
            }
            _ => {
                let mut external = principal(Uuid::new_v4());
                external.provider = "entra".into();
                auth.sign_in_external(&external, Some(handle))
                    .await
                    .unwrap();
            }
        }
        let remaining: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM auth.user_account WHERE user_id = $1)",
        )
        .bind(expired.user_id)
        .fetch_one(&world.pool)
        .await
        .unwrap();
        assert!(
            !remaining,
            "expired account is removed before reclaiming {handle}"
        );
    }
}

#[tokio::test]
async fn enrollment_preserves_admission_and_management_is_rate_limited_without_exposing_codes() {
    let (world, auth, routes, owner) = setup().await;
    assert_eq!(
        world
            .auth
            .reserve_agent(caller(&owner), "provider_off", "No")
            .await
            .err(),
        Some(AuthError::EnrollmentNotPermitted)
    );
    let reservation = auth
        .reserve_agent(caller(&owner), "admitted_agent", "Agent")
        .await
        .unwrap();
    let stored: (Vec<u8>, f64, Uuid) = sqlx::query_as(
        "SELECT code_hash, extract(epoch FROM (expires_at - clock_timestamp()))::float8, \
             consent_device_id FROM auth.agent_enrollment WHERE user_id=$1",
    )
    .bind(reservation.user_id)
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert_eq!(
        stored.0,
        crate::tokens::refresh_hash(&reservation.enrollment_code)
    );
    assert!(
        stored.1 > 598.0 && stored.1 <= 600.0,
        "exact 600-second database TTL"
    );
    assert_eq!(stored.2, owner.device_id);
    let denied = router(Api {
        auth: auth
            .clone()
            .with_admission(super::RequireGroup::new("not-present")),
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: None,
        directory: Arc::new(NoDirectory),
    });
    let token = app_token(&world, TENANT, Uuid::new_v4());
    let (status, _) = call(
        &denied,
        "POST",
        "/v1/auth/entra-app",
        "",
        json!({"token":token,"enrollment_code":reservation.enrollment_code}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &routes,
        "POST",
        "/v1/auth/entra-app",
        "",
        json!({"token":token,"enrollment_code":reservation.enrollment_code}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "refused admission did not consume consent"
    );
    for _ in 0..120 {
        assert_eq!(
            call(&routes, "GET", "/v1/agents", &owner.access_token, json!({}))
                .await
                .0,
            StatusCode::OK
        );
    }
    let (status, error) = call(&routes, "GET", "/v1/agents", &owner.access_token, json!({})).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(error["error"]["code"], "rate_limited");
}

pub(super) async fn setup() -> (World, Auth, Router, Tokens) {
    let world = world(Offer::Both).await;
    let mut settings = world.stub.settings();
    settings.tenants = crate::entra::Tenants::Ids(vec![TENANT.parse().unwrap()]);
    let app = crate::entra::EntraSignIn::new(settings)
        .unwrap()
        .app(EntraAppSettings::default())
        .unwrap();
    let auth = world.auth.clone().with_entra_app(app);
    let owner = password_user(&auth, &world.pool, "owner", 1)
        .await
        .remove(0);
    let router = router(Api {
        auth: auth.clone(),
        limits: Limits::new(Arc::new(MemoryCounter::default())),
        client_ip_header: None,
        directory: Arc::new(NoDirectory),
    });
    (world, auth, router, owner)
}

#[tokio::test]
async fn disabling_enrollment_also_refuses_previously_reserved_codes_without_spending_them() {
    let (_world, auth, _, owner) = setup().await;
    let reserved = auth
        .reserve_agent(caller(&owner), "policy_agent", "Policy")
        .await
        .unwrap();
    let app = principal(Uuid::new_v4());
    let disabled = auth.clone().with_agents_per_user(0);
    assert_eq!(
        disabled
            .enroll_agent(&app, &reserved.enrollment_code)
            .await
            .err(),
        Some(AuthError::EnrollmentNotPermitted)
    );
    assert!(
        auth.enroll_agent(&app, &reserved.enrollment_code)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_legacy_linked_app_is_not_a_human_owner_even_with_the_old_default_kind() {
    let (world, auth, _, _) = setup().await;
    let user = Uuid::now_v7();
    let app = principal(Uuid::new_v4());
    sqlx::query(
        "INSERT INTO auth.user_account (user_id,handle,display_name,email_verified_at) \
         VALUES ($1,'legacy_app','Legacy app',now())",
    )
    .bind(user)
    .execute(&world.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auth.external_identity (identity_id,provider,issuer,subject,user_id) \
         VALUES ($1,'entra-app',$2,$3,$4)",
    )
    .bind(Uuid::now_v7())
    .bind(&app.issuer)
    .bind(&app.subject)
    .bind(user)
    .execute(&world.pool)
    .await
    .unwrap();
    let signed_in = auth.sign_in_external(&app, None).await.unwrap();
    let device = auth
        .register_device(
            &signed_in.registration.token,
            Uuid::now_v7(),
            "Legacy",
            "other",
        )
        .await
        .unwrap();
    assert_eq!(
        auth.reserve_agent(caller(&device), "nested_legacy_agent", "Nested")
            .await
            .err(),
        Some(AuthError::EnrollmentNotPermitted)
    );
    assert_eq!(
        auth.owned_agents(caller(&device)).await.err(),
        Some(AuthError::EnrollmentNotPermitted)
    );
}

#[tokio::test]
async fn same_app_competing_across_distinct_owners_has_one_live_binding() {
    let (world, auth, _, owner) = setup().await;
    let other_owner = password_user(&auth, &world.pool, "another_owner", 1)
        .await
        .remove(0);
    let a = auth
        .reserve_agent(caller(&owner), "owner_one_agent", "One")
        .await
        .unwrap();
    let b = auth
        .reserve_agent(caller(&other_owner), "owner_two_agent", "Two")
        .await
        .unwrap();
    let app = principal(Uuid::new_v4());
    let (a, b) = tokio::join!(
        auth.enroll_agent(&app, &a.enrollment_code),
        auth.enroll_agent(&app, &b.enrollment_code)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(
        a.as_ref().err().or(b.as_ref().err()),
        Some(&AuthError::IdentityInUse)
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM auth.external_identity WHERE provider='entra-app'",
    )
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

pub(super) async fn call(
    router: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if !token.is_empty() {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = router
        .clone()
        .oneshot(
            request
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

fn app_token(world: &World, tenant: &str, object: Uuid) -> String {
    let mut claims = world.stub.user_claims(tenant, &object.to_string());
    claims.as_object_mut().unwrap().remove("scp");
    claims["idtyp"] = json!("app");
    claims["azp"] = json!(CLIENT_APP);
    sign(KEY_A, &claims)
}

#[tokio::test]
async fn owner_reserves_two_agents_with_ten_minute_single_use_codes() {
    let (world, _auth, router, owner) = setup().await;
    for handle in ["agent_one", "agent_two"] {
        let (status, reservation) = call(
            &router,
            "POST",
            "/v1/agents",
            &owner.access_token,
            json!({"handle": handle, "display_name": "My agent"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{reservation}");
        assert_eq!(reservation["expires_in_s"], 600);
        assert!(reservation["enrollment_code"].as_str().unwrap().len() >= 43);
        let id: Uuid = reservation["user_id"].as_str().unwrap().parse().unwrap();
        assert_ne!(id, owner.user_id);
        let (_, list) = call(&router, "GET", "/v1/agents", &owner.access_token, json!({})).await;
        assert!(
            !list
                .to_string()
                .contains(reservation["enrollment_code"].as_str().unwrap())
        );
    }
    let (status, error) = call(
        &router,
        "POST",
        "/v1/agents",
        &owner.access_token,
        json!({"handle":"agent_three","display_name":"Third"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["error"]["code"], "too_many_agents");
    assert_eq!(world.accounts().await, 3);
    let (_, list) = call(&router, "GET", "/v1/agents", &owner.access_token, json!({})).await;
    assert_eq!(list["agents"].as_array().unwrap().len(), 2);
    assert_eq!(list["agents"][0]["state"], "pending");
}

#[tokio::test]
async fn consent_binds_a_separate_account_and_deletion_disables_it() {
    let (world, auth, router, owner) = setup().await;
    let (status, reservation) = call(
        &router,
        "POST",
        "/v1/agents",
        &owner.access_token,
        json!({"handle":"my_agent","display_name":"My agent"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let token = app_token(&world, TENANT, Uuid::new_v4());
    let (status, registered) = call(
        &router,
        "POST",
        "/v1/auth/entra-app",
        "",
        json!({"token":token,"enrollment_code":reservation["enrollment_code"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{registered}");
    assert_eq!(registered["user_id"], reservation["user_id"]);
    let device = auth
        .register_device(
            registered["registration_token"].as_str().unwrap(),
            Uuid::now_v7(),
            "Agent process",
            "other",
        )
        .await
        .unwrap();
    let (_, own) = call(
        &router,
        "GET",
        "/v1/users/me",
        &device.access_token,
        json!({}),
    )
    .await;
    assert_eq!(own["kind"], "agent");
    assert_eq!(own["owner_user_id"], owner.user_id.to_string());
    let (_, forbidden) = call(
        &router,
        "POST",
        "/v1/agents",
        &device.access_token,
        json!({"handle":"nested_agent","display_name":"No"}),
    )
    .await;
    assert_eq!(forbidden["error"]["code"], "enrollment_not_permitted");
    let path = format!("/v1/agents/{}", device.user_id);
    let (status, _) = call(&router, "DELETE", &path, &owner.access_token, json!({})).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(auth.refresh(&device.refresh_token).await.is_err());
    assert!(auth.authenticate(&device.access_token).await.is_err());
    assert!(
        auth.register_device(
            registered["registration_token"].as_str().unwrap(),
            Uuid::now_v7(),
            "Old token",
            "other"
        )
        .await
        .is_err()
    );
    let (status, _) = call(
        &router,
        "POST",
        "/v1/auth/entra-app",
        "",
        json!({"token":token}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, lookup) = call(
        &router,
        "GET",
        "/v1/users/lookup?handle=my_agent",
        &owner.access_token,
        json!({}),
    )
    .await;
    assert_eq!(lookup["error"]["code"], "user_not_found");
    let events = world.outbox().await;
    assert_eq!(
        events
            .iter()
            .filter(|(kind, _)| kind == "agent.enrolled")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|(kind, _)| kind == "device.revoked")
            .count(),
        1
    );
}

#[tokio::test]
async fn expired_reservations_release_handles_even_to_password_signup() {
    let (world, auth, _, owner) = setup().await;
    let reservation = auth
        .reserve_agent(caller(&owner), "free_handle", "Pending")
        .await
        .unwrap();
    sqlx::query("UPDATE auth.agent_enrollment SET expires_at = now() - interval '1 second' WHERE user_id = $1")
        .bind(reservation.user_id).execute(&world.pool).await.unwrap();
    assert!(
        auth.enroll_agent(&principal(Uuid::new_v4()), &reservation.enrollment_code)
            .await
            .is_err()
    );
    assert!(
        auth.signup(Signup {
            email: "another@example.com".into(),
            password: "correct horse".into(),
            handle: "free_handle".into(),
            display_name: "Human".into(),
        })
        .await
        .is_ok(),
        "an expired reservation must not squat on a handle"
    );
}

#[tokio::test]
async fn cap_is_atomic_across_replicas_and_respects_zero_and_configuration_changes() {
    let (world, auth, _, owner) = setup().await;
    let mut requests = tokio::task::JoinSet::new();
    for i in 0..12 {
        let auth = auth.clone();
        let owner = caller(&owner);
        requests.spawn(async move {
            auth.reserve_agent(owner, &format!("race_{i}"), "Race")
                .await
                .map(|r| r.user_id)
        });
    }
    let mut reserved = Vec::new();
    while let Some(result) = requests.join_next().await {
        match result.unwrap() {
            Ok(id) => reserved.push(id),
            Err(error) => assert_eq!(error, AuthError::TooManyAgents),
        }
    }
    assert_eq!(reserved.len(), 2);
    assert_eq!(
        auth.clone()
            .with_agents_per_user(0)
            .reserve_agent(caller(&owner), "disabled_agent", "No")
            .await
            .err(),
        Some(AuthError::TooManyAgents)
    );
    let increased = auth.clone().with_agents_per_user(3);
    assert!(
        increased
            .reserve_agent(caller(&owner), "third_agent", "Third")
            .await
            .is_ok()
    );
    sqlx::query("UPDATE auth.agent_enrollment SET expires_at = now() - interval '1 second'")
        .execute(&world.pool)
        .await
        .unwrap();
    assert!(auth.owned_agents(caller(&owner)).await.unwrap().is_empty());
    assert!(
        auth.reserve_agent(caller(&owner), "race_0", "Reclaimed")
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn concurrent_code_redemption_and_duplicate_app_never_consume_an_unrelated_code() {
    let (world, auth, _, owner) = setup().await;
    let first = auth
        .reserve_agent(caller(&owner), "first_agent", "First")
        .await
        .unwrap();
    let second = auth
        .reserve_agent(caller(&owner), "second_agent", "Second")
        .await
        .unwrap();
    let app = principal(Uuid::new_v4());
    let (a, b) = tokio::join!(
        auth.enroll_agent(&app, &first.enrollment_code),
        auth.enroll_agent(&app, &second.enrollment_code)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(
        a.as_ref().err().or(b.as_ref().err()),
        Some(&AuthError::IdentityInUse)
    );
    let (spent, unspent) = if a.is_ok() {
        (&first, &second)
    } else {
        (&second, &first)
    };
    assert_eq!(
        auth.enroll_agent(&principal(Uuid::new_v4()), &spent.enrollment_code)
            .await
            .err(),
        Some(AuthError::EnrollmentInvalid)
    );
    let other = principal(Uuid::new_v4());
    let third = principal(Uuid::new_v4());
    let (a, b) = tokio::join!(
        auth.enroll_agent(&other, &unspent.enrollment_code),
        auth.enroll_agent(&third, &unspent.enrollment_code)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(
        a.as_ref().err().or(b.as_ref().err()),
        Some(&AuthError::EnrollmentInvalid)
    );
    assert_eq!(
        auth.enroll_agent(&principal(Uuid::new_v4()), "bad-code")
            .await
            .err(),
        Some(AuthError::EnrollmentInvalid)
    );
    assert_eq!(world.accounts().await, 3);
    assert_eq!(
        world
            .outbox()
            .await
            .iter()
            .filter(|(k, _)| k == "agent.enrolled")
            .count(),
        2
    );
}

#[tokio::test]
async fn management_is_owner_scoped_and_profiles_validate_names_and_hide_pending_users() {
    let (world, auth, router, owner) = setup().await;
    let stranger = password_user(&auth, &world.pool, "stranger", 1)
        .await
        .remove(0);
    let pending = auth
        .reserve_agent(caller(&owner), "pending_agent", "Pending")
        .await
        .unwrap();
    assert_eq!(
        auth.lookup("pending_agent").await,
        Err(AuthError::UserNotFound)
    );
    assert!(auth.profiles(&[pending.user_id]).await.unwrap().is_empty());
    let path = format!("/v1/agents/{}", pending.user_id);
    for method in ["PATCH", "DELETE"] {
        let (status, _) = call(
            &router,
            method,
            &path,
            &stranger.access_token,
            json!({"handle":"stolen_agent"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(
            &router,
            method,
            &format!("/v1/agents/{}", owner.user_id),
            &owner.access_token,
            json!({"handle":"not_an_agent"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    assert!(
        auth.owned_agents(caller(&stranger))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        auth.change_agent(caller(&owner), pending.user_id, Some("x"), None, false)
            .await,
        Err(AuthError::Invalid(_))
    ));
    assert_eq!(
        auth.change_agent(caller(&owner), pending.user_id, Some("owner"), None, false)
            .await,
        Err(AuthError::HandleTaken)
    );
    auth.change_agent(
        caller(&owner),
        pending.user_id,
        Some("@Renamed_Agent"),
        Some("Renamed"),
        false,
    )
    .await
    .unwrap();
    let signed_in = auth
        .enroll_agent(&principal(Uuid::new_v4()), &pending.enrollment_code)
        .await
        .unwrap();
    let profile = auth.lookup("renamed_agent").await.unwrap();
    assert_eq!(profile.user_id, signed_in.user_id);
    assert_eq!(profile.kind, "agent");
    assert_eq!(profile.owner_user_id, Some(owner.user_id));
    assert_eq!(
        auth.profiles(&[owner.user_id, signed_in.user_id])
            .await
            .unwrap()
            .len(),
        2
    );
    let second = auth
        .reserve_agent(caller(&owner), "delete_pending", "Pending")
        .await
        .unwrap();
    auth.change_agent(caller(&owner), second.user_id, None, None, true)
        .await
        .unwrap();
    assert!(
        auth.reserve_agent(caller(&owner), "delete_pending", "New")
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn tenant_consent_is_exact_for_entra_and_unambiguous_for_password_owners() {
    let (world, auth, _, password) = setup().await;
    let multi = auth.clone().with_entra_app(
        world
            .auth
            .entra()
            .unwrap()
            .app(EntraAppSettings::default())
            .unwrap(),
    );
    assert_eq!(
        multi
            .reserve_agent(caller(&password), "ambiguous_agent", "No")
            .await
            .err(),
        Some(AuthError::EnrollmentNotPermitted)
    );
    let entra = super::entra_user(&multi, Uuid::new_v4()).await;
    let reservation = multi
        .reserve_agent(caller(&entra), "tenant_agent", "Tenant")
        .await
        .unwrap();
    let mut foreign = principal(Uuid::new_v4());
    foreign.issuer = crate::entra::testing::OTHER_TENANT.into();
    assert_eq!(
        multi
            .enroll_agent(&foreign, &reservation.enrollment_code)
            .await
            .err(),
        Some(AuthError::EnrollmentNotPermitted)
    );
    assert!(
        multi
            .enroll_agent(&principal(Uuid::new_v4()), &reservation.enrollment_code)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn agent_devices_have_an_independent_limit_session_and_admin_external_selector() {
    let (world, auth, _, owner) = setup().await;
    let reservation = auth
        .reserve_agent(caller(&owner), "device_agent", "Devices")
        .await
        .unwrap();
    let app = principal(Uuid::new_v4());
    let registration = auth
        .enroll_agent(&app, &reservation.enrollment_code)
        .await
        .unwrap();
    let mut devices = Vec::new();
    for _ in 0..10 {
        devices.push(
            auth.register_device(
                &registration.registration.token,
                Uuid::now_v7(),
                "Worker",
                "other",
            )
            .await
            .unwrap(),
        );
    }
    assert_eq!(
        auth.register_device(
            &registration.registration.token,
            Uuid::now_v7(),
            "Eleventh",
            "other"
        )
        .await,
        Err(AuthError::TooManyDevices)
    );
    assert!(devices[0].refresh_expires_in_s <= 24 * 3600);
    assert!(auth.authenticate(&owner.access_token).await.is_ok());
    let renewed = auth
        .sign_in_external_device(&app, None, devices[0].device_id)
        .await
        .unwrap();
    assert_eq!(renewed.device_id, devices[0].device_id);
    assert_eq!(
        auth.refresh(&devices[0].refresh_token).await,
        Err(AuthError::Unauthorized)
    );
    sqlx::query("UPDATE auth.device SET session_expires_at = 1 WHERE device_id = $1")
        .bind(renewed.device_id)
        .execute(&world.pool)
        .await
        .unwrap();
    assert_eq!(
        auth.refresh(&renewed.refresh_token).await,
        Err(AuthError::SessionExpired)
    );
    let admin = crate::admin_router(crate::AdminApi::new(
        auth.clone(),
        Limits::new(Arc::new(MemoryCounter::default())),
        "admin-test",
    ));
    let (status, result) = call(
        &admin,
        "POST",
        "/v1/admin/users/revoke-sessions",
        "admin-test",
        json!({"external":{"provider":"entra-app","issuer":TENANT,"subject":app.subject}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    for device in devices {
        assert!(auth.authenticate(&device.access_token).await.is_err());
        assert!(auth.refresh(&device.refresh_token).await.is_err());
    }
    assert!(auth.authenticate(&owner.access_token).await.is_ok());
    assert!(
        auth.sign_in_external(&app, None).await.is_ok(),
        "admin revoke is not deletion"
    );
}

#[tokio::test]
async fn deletion_serializes_with_registration_refresh_and_reauthentication() {
    use std::time::Duration;

    for operation in ["registration", "refresh", "reauthentication"] {
        for deletion_first in [true, false] {
            let (world, auth, _, owner) = setup().await;
            let reservation = auth
                .reserve_agent(caller(&owner), "racing_agent", "Race")
                .await
                .unwrap();
            let app = principal(Uuid::new_v4());
            let registration = auth
                .enroll_agent(&app, &reservation.enrollment_code)
                .await
                .unwrap();
            let device = auth
                .register_device(
                    &registration.registration.token,
                    Uuid::now_v7(),
                    "Worker",
                    "other",
                )
                .await
                .unwrap();
            let mut holder = world.pool.begin().await.unwrap();
            sqlx::query("SELECT 1 FROM auth.user_account WHERE user_id = $1 FOR UPDATE")
                .bind(device.user_id)
                .execute(&mut *holder)
                .await
                .unwrap();

            let deleting = auth.clone();
            let owner_caller = caller(&owner);
            let agent_id = device.user_id;
            let delete = async move {
                deleting
                    .change_agent(owner_caller, agent_id, None, None, true)
                    .await
            };
            let issuing = auth.clone();
            let app_identity = app.clone();
            let initial = device.clone();
            let issue = async move {
                match operation {
                    "registration" => {
                        issuing
                            .register_device(
                                &registration.registration.token,
                                Uuid::now_v7(),
                                "Race",
                                "other",
                            )
                            .await
                    }
                    "refresh" => issuing.refresh(&initial.refresh_token).await,
                    "reauthentication" => {
                        issuing
                            .sign_in_external_device(&app_identity, None, initial.device_id)
                            .await
                    }
                    _ => unreachable!(),
                }
            };
            // Observe each waiter before starting the next, forcing both lock orders.
            let (deletion, issuance) = if deletion_first {
                let deletion = tokio::spawn(delete);
                wait_for_account_waiters(&world.pool, 1).await;
                let issuance = tokio::spawn(issue);
                (deletion, issuance)
            } else {
                let issuance = tokio::spawn(issue);
                wait_for_account_waiters(&world.pool, 1).await;
                let deletion = tokio::spawn(delete);
                (deletion, issuance)
            };
            wait_for_account_waiters(&world.pool, 2).await;
            holder.commit().await.unwrap();
            let (deleted, issued) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(deletion, issuance)
            })
            .await
            .expect("deletion and token issuance finish without a lock cycle");
            deleted.unwrap().unwrap();
            match issued.unwrap() {
                Ok(tokens) if !deletion_first => {
                    assert_eq!(
                        auth.authenticate(&tokens.access_token).await,
                        Err(AuthError::Unauthorized)
                    );
                    assert_eq!(
                        auth.refresh(&tokens.refresh_token).await,
                        Err(AuthError::Unauthorized)
                    );
                }
                Err(AuthError::Unauthorized) if deletion_first => {}
                unexpected => panic!(
                    "{operation}, deletion_first={deletion_first}: unexpected result {unexpected:?}"
                ),
            }
            let live: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM auth.device WHERE user_id = $1 AND revoked_at IS NULL",
            )
            .bind(device.user_id)
            .fetch_one(&world.pool)
            .await
            .unwrap();
            assert_eq!(live, 0);
            assert_eq!(
                auth.enroll_agent(&app, &reservation.enrollment_code)
                    .await
                    .err(),
                Some(AuthError::EnrollmentInvalid),
                "the deleted agent's code stays spent"
            );
            let replacement = auth
                .reserve_agent(caller(&owner), "replacement_agent", "Replacement")
                .await
                .unwrap();
            assert!(
                auth.enroll_agent(&app, &replacement.enrollment_code)
                    .await
                    .is_ok(),
                "deletion released the app"
            );
        }
    }
}

#[tokio::test]
async fn agents_may_reserve_and_take_hyphenated_handles() {
    let (_world, auth, router, owner) = setup().await;
    let (status, reserved) = call(
        &router,
        "POST",
        "/v1/agents",
        &owner.access_token,
        json!({"handle": "@Build-Bot", "display_name": "Build bot"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{reserved}");
    let user_id: Uuid = reserved["user_id"].as_str().unwrap().parse().unwrap();
    let (_, list) = call(&router, "GET", "/v1/agents", &owner.access_token, json!({})).await;
    assert_eq!(list["agents"][0]["handle"], "build-bot");
    let (status, _) = call(
        &router,
        "PATCH",
        &format!("/v1/agents/{user_id}"),
        &owner.access_token,
        json!({"handle": "deploy-bot-2"}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = call(&router, "GET", "/v1/agents", &owner.access_token, json!({})).await;
    assert_eq!(list["agents"][0]["handle"], "deploy-bot-2");
    for bad in ["deploy-", "deploy--bot"] {
        assert!(matches!(
            auth.change_agent(caller(&owner), user_id, Some(bad), None, false)
                .await,
            Err(AuthError::Invalid(_))
        ));
    }
}

#[tokio::test]
async fn deleting_an_agent_releases_its_app_for_a_new_enrollment() {
    let (world, auth, router, owner) = setup().await;
    let object = Uuid::new_v4();
    let token = app_token(&world, TENANT, object);
    let first = auth
        .reserve_agent(caller(&owner), "first_bot", "First")
        .await
        .unwrap();
    let (status, registered) = call(
        &router,
        "POST",
        "/v1/auth/entra-app",
        "",
        json!({"token":token,"enrollment_code":first.enrollment_code}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{registered}");
    let registration = registered["registration_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let device = auth
        .register_device(&registration, Uuid::now_v7(), "Bot", "other")
        .await
        .unwrap();
    let path = format!("/v1/agents/{}", first.user_id);
    let (status, _) = call(&router, "DELETE", &path, &owner.access_token, json!({})).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The released app is simply unenrolled: never the disabled account, never a new one.
    let accounts = world.accounts().await;
    let (status, refused) = call(
        &router,
        "POST",
        "/v1/auth/entra-app",
        "",
        json!({"token":token}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"]["code"], "sign_in_not_permitted");
    assert_eq!(world.accounts().await, accounts);
    assert!(auth.authenticate(&device.access_token).await.is_err());
    assert!(auth.refresh(&device.refresh_token).await.is_err());
    assert_eq!(
        auth.register_device(&registration, Uuid::now_v7(), "Old", "other")
            .await
            .err(),
        Some(AuthError::Unauthorized)
    );
    let released: (String, String, String, bool) = sqlx::query_as(
        "SELECT provider, issuer, subject, released_at >= created_at \
         FROM auth.released_identity WHERE user_id = $1",
    )
    .bind(first.user_id)
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert_eq!(
        released,
        ("entra-app".into(), TENANT.into(), object.to_string(), true)
    );
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM auth.external_identity WHERE user_id = $1")
            .bind(first.user_id)
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert_eq!(live, 0);

    let second = auth
        .reserve_agent(caller(&owner), "second_bot", "Second")
        .await
        .unwrap();
    let (status, rebound) = call(
        &router,
        "POST",
        "/v1/auth/entra-app",
        "",
        json!({"token":token,"enrollment_code":second.enrollment_code}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rebound}");
    assert_eq!(rebound["user_id"], second.user_id.to_string());
    let (_, list) = call(&router, "GET", "/v1/agents", &owner.access_token, json!({})).await;
    let agents = list["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["user_id"], second.user_id.to_string());
    assert_eq!(agents[0]["state"], "active");
    let disabled: bool = sqlx::query_scalar(
        "SELECT disabled_at IS NOT NULL FROM auth.user_account WHERE user_id = $1",
    )
    .bind(first.user_id)
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert!(disabled, "the deleted agent stays disabled");
    // Without a code the app now signs in to the new agent only.
    let (status, again) = call(
        &router,
        "POST",
        "/v1/auth/entra-app",
        "",
        json!({"token":token}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["user_id"], second.user_id.to_string());
    // And it still cannot be bound to a second live agent.
    let third = auth
        .reserve_agent(caller(&owner), "third_bot", "Third")
        .await
        .unwrap();
    let (status, in_use) = call(
        &router,
        "POST",
        "/v1/auth/entra-app",
        "",
        json!({"token":token,"enrollment_code":third.enrollment_code}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{in_use}");
    assert_eq!(in_use["error"]["code"], "identity_in_use");
}
