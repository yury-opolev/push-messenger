# Release an agent's identity on deletion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deleting an active agent releases all its external identities so the same app can enroll again (REQUEST PUSH feature 7).

**Architecture:** In `Auth::change_agent`'s delete transaction, after `disabled_at` is set and devices are revoked, a provider-agnostic helper moves every `auth.external_identity` row of the agent into a new append-only audit table `auth.released_identity` (migration 0008) and logs one `auth.agent.identity_released` security line per row. Spec: `docs/superpowers/specs/2026-10-26-release-agent-identity-on-delete-design.md`.

**Tech Stack:** Rust, axum, sqlx (PostgreSQL 17), testcontainers (Docker).

**Commands** (run from the worktree root; set `$env:CARGO_TARGET_DIR = "C:\Users\yurio\Documents\github\push-messenger\target"` in the same PowerShell call):
- After adding a migration: `cargo clean -p push-auth-core` (no build.rs rerun-if-changed).
- Tests: `cargo test -p push-auth-core --locked -j 4 <filter>`
- Gates: `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`

---

### Task 1: Migration 0008 and the release on deletion

**Files:**
- Create: `crates/push-auth-core/migrations/0008_released_identity.sql`
- Modify: `crates/push-auth-core/src/agents.rs` (module doc line 1; new `release_identities`; `change_agent` delete branch)
- Modify: `crates/push-auth-core/tests/schema.rs` (table list in `the_owner_migrates_and_a_second_run_changes_nothing`)
- Test: `crates/push-auth-core/src/tests/agents.rs`

- [ ] **Step 1: Write the failing tests** (append to `src/tests/agents.rs`)

```rust
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
    let registration = registered["registration_token"].as_str().unwrap().to_owned();
    let device = auth
        .register_device(&registration, Uuid::now_v7(), "Bot", "other")
        .await
        .unwrap();
    let path = format!("/v1/agents/{}", first.user_id);
    let (status, _) = call(&router, "DELETE", &path, &owner.access_token, json!({})).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The released app is simply unenrolled: never the disabled account, never a new one.
    let accounts = world.accounts().await;
    let (status, refused) = call(&router, "POST", "/v1/auth/entra-app", "", json!({"token":token})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"]["code"], "sign_in_not_permitted");
    assert_eq!(world.accounts().await, accounts);
    assert!(auth.authenticate(&device.access_token).await.is_err());
    assert!(auth.refresh(&device.refresh_token).await.is_err());
    assert_eq!(
        auth.register_device(&registration, Uuid::now_v7(), "Old", "other").await.err(),
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
    assert_eq!(released, ("entra-app".into(), TENANT.into(), object.to_string(), true));
    let live: i64 = sqlx::query_scalar("SELECT count(*) FROM auth.external_identity WHERE user_id = $1")
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
    let disabled: bool = sqlx::query_scalar("SELECT disabled_at IS NOT NULL FROM auth.user_account WHERE user_id = $1")
        .bind(first.user_id)
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert!(disabled, "the deleted agent stays disabled");
    // Without a code the app now signs in to the new agent only.
    let (status, again) = call(&router, "POST", "/v1/auth/entra-app", "", json!({"token":token})).await;
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
```

Also update the existing tests whose expectations encode the old "permanent tombstone":
- `deletion_serializes_with_registration_refresh_and_reauthentication`: replace the final
  `IdentityInUse` assertion with:

```rust
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
```

- Rename `consent_binds_a_separate_account_and_deletion_is_a_permanent_tombstone` to
  `consent_binds_a_separate_account_and_deletion_disables_it` (assertions unchanged: 403 for
  the released token still holds).
- Rename `same_app_competing_across_distinct_owners_has_one_permanent_binding` to
  `same_app_competing_across_distinct_owners_has_one_live_binding`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p push-auth-core --locked -j 4 deleting_an_agent_releases deletion_serializes`
Expected: FAIL (`relation "auth.released_identity" does not exist`, then `IdentityInUse` on rebinding).

- [ ] **Step 3: Migration** `crates/push-auth-core/migrations/0008_released_identity.sql`

```sql
-- Deleting an agent releases its identities (REQUEST PUSH feature 7): each row of
-- auth.external_identity is moved here, so the same app (or key) can be enrolled again
-- while the binding stays auditable. Append-only; one identity may be released many times.
-- Enrollment facts (tenant, client app, consent device, time) stay in auth.agent_enrollment.
CREATE TABLE auth.released_identity (
    identity_id uuid PRIMARY KEY,
    provider    text NOT NULL,
    issuer      text NOT NULL,
    subject     text NOT NULL,
    user_id     uuid NOT NULL REFERENCES auth.user_account,
    created_at  timestamptz NOT NULL,
    released_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX released_identity_key ON auth.released_identity (provider, issuer, subject);
CREATE INDEX released_identity_user ON auth.released_identity (user_id);
```

Then `cargo clean -p push-auth-core`.

- [ ] **Step 4: Helper + call site** in `src/agents.rs`

Module doc line 1 becomes:
`//! Owner consent, bounded reservations, and agent deletion that releases the app identity.`

Add after `owner()`:

```rust
/// Moves every external identity of `agent` into `auth.released_identity` (spec R1-R3),
/// whatever its provider, so the identity can be enrolled again; the account keeps its
/// history. Call inside the transaction that disables the account.
async fn release_identities(
    connection: &mut PgConnection,
    agent: Uuid,
    owner_user_id: Uuid,
) -> Result<(), AuthError> {
    let released: Vec<(String, String, String, Option<Uuid>)> = sqlx::query_as(
        "WITH gone AS (DELETE FROM auth.external_identity WHERE user_id = $1 \
         RETURNING identity_id, provider, issuer, subject, user_id, created_at), \
         kept AS (INSERT INTO auth.released_identity \
         (identity_id, provider, issuer, subject, user_id, created_at) \
         SELECT identity_id, provider, issuer, subject, user_id, created_at FROM gone \
         RETURNING provider, issuer, subject, user_id) \
         SELECT k.provider, k.issuer, k.subject, e.client_app_id FROM kept k \
         LEFT JOIN auth.agent_enrollment e USING (user_id) ORDER BY k.provider, k.issuer, k.subject",
    )
    .bind(agent)
    .fetch_all(connection)
    .await?;
    for (provider, issuer, subject, client_app_id) in released {
        tracing::info!(
            target: "security", event = "auth.agent.identity_released", user_id = %agent,
            %owner_user_id, %provider, %issuer, %subject, ?client_app_id,
            "a deleted agent's identity was released"
        );
    }
    Ok(())
}
```

In `change_agent`, inside `if active { ... }`, after `revoke_devices(&mut tx, &devices).await?;`:

```rust
                release_identities(&mut tx, agent, caller.user_id).await?;
```

- [ ] **Step 5: Schema test** — in `tests/schema.rs` the expected table list becomes
`["agent_enrollment","device","external_identity","outbox","refresh_token","released_identity","user_account"]`.

- [ ] **Step 6: Run tests**

Run: `cargo test -p push-auth-core --locked -j 4`
Expected: all pass.

- [ ] **Step 7: Gates + commit**

`cargo fmt --all`; `cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`.
Commit: `feat(auth): release an agent's identities when it is deleted (migration 0008)`.

### Task 2: Documentation and release notes

**Files:**
- Modify: `docs/entra-sign-in.md` (enrollment step 6, `DELETE` table row, tombstone paragraph)
- Modify: `CHANGELOG.md` (Unreleased → Changed)
- Modify: `docs/superpowers/specs/2026-10-19-entra-sign-in-design.md` D5: "Disabled bindings are permanent tombstones." → "Deleting an active agent releases its identity (see 2026-10-26 release design)."

- [ ] **Step 1: docs/entra-sign-in.md**
  - Step 6 last sentence: "A second enrollment of an `(entra-app,tid,oid)` that is bound to a live agent returns **409 `identity_in_use`**, without consuming an unrelated reservation; once that agent is deleted the app may enroll again."
  - DELETE row: "204; removes a pending reservation, or permanently disables the active agent and releases its app identity".
  - Replace "Disabled bindings are tombstones: sign-in cannot revive them, even with a fresh code; old registration, access and refresh tokens are refused." with: "Deleted agents stay disabled forever: nothing revives the account, and its old registration, access and refresh tokens are refused. Deletion **releases** the agent's app identity in the same transaction: the `(entra-app,tid,oid)` row moves to the audit table `auth.released_identity` (with `released_at`; tenant and `client_app_id` stay on `auth.agent_enrollment`) and a `security` log line `auth.agent.identity_released` names user, owner, tenant, subject and client app. The released app is then unenrolled: without a code it gets 403 `sign_in_not_permitted`, and with a fresh code (any owner) it enrolls a new agent. Only owner deletion disables an account; admin revoke-sessions is a sign-out and keeps the identity." Keep the tombstone wording for the profile-edit sentence ("deleted agent's display name").

- [ ] **Step 2: CHANGELOG.md** — add at the top of `### Changed`:

```markdown
- **Deleting an agent releases its app identity** (REQUEST PUSH feature 7). `DELETE
  /v1/agents/{agent_id}` of an active agent still disables it for good and revokes its
  devices and tokens, and now also releases its app registration or managed identity in
  the same transaction: a fresh `POST /v1/auth/entra-app {token, enrollment_code}` with
  that app enrolls a new agent (of any owner) instead of failing with `identity_in_use`.
  Until then the released app gets the normal unenrolled 403 `sign_in_not_permitted`.
  Two live agents still cannot share an app. The released binding is kept in the new
  `auth.released_identity` table and logged as `auth.agent.identity_released`. Admin
  revoke-sessions is unchanged and does not release.
- auth migration `0008_released_identity.sql` adds the `auth.released_identity` audit
  table. It changes no existing data and runs with the usual auth migrations.
```

- [ ] **Step 3: Commit** `docs: release notes for releasing an agent's identity on deletion`.
