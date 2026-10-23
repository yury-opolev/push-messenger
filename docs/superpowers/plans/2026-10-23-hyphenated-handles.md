# Hyphenated handles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Accept `-` in handles (REQUEST PUSH 42) everywhere a handle is accepted, with the
database CHECK in step.

**Architecture:** `push_auth_core::rules::handle` is the single API-side check (signup,
Entra/external sign-in, profile updates, lookup, admin API, agent reservation and rename).
Widen it, then add sqlx migration `0007` replacing `auth.user_account`'s handle CHECK with a
PostgreSQL-compatible equivalent (no lookahead). Spec:
[2026-10-23-hyphenated-handles-design.md](../specs/2026-10-23-hyphenated-handles-design.md).

**Tech Stack:** Rust 2024 (MSRV 1.85), sqlx migrations, PostgreSQL 17 (testcontainers;
Docker must run), axum.

**Rule:** after trim, optional leading `@` stripped, lower-cased: 3–30 characters, first
`a-z`, then `a-z0-9_-`, not ending with `-`, no `--`.

**Commands** (run from the worktree root; to keep disk use modest, share one target dir:
`$env:CARGO_TARGET_DIR = 'C:\Users\yurio\Documents\github\push-messenger\target'`, and pass
`-j 4`):

- Unit: `cargo test -p push-auth-core --lib rules -j 4`
- Schema: `cargo test -p push-auth-core --test schema -j 4`
- API: `cargo test -p push-auth-core --test api -j 4` and `cargo test -p push-auth-core --lib tests::agents -j 4`
- Gates: `cargo fmt --all -- --check`; `cargo clippy -p push-auth-core --all-targets --locked -j 4 -- -D warnings`

Commits: conventional (`feat(auth): …`, `docs: …`), ending with the trailer
`Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>`. Never commit
`.github/copilot/`.

---

### Task 1: The handle rule

**Files:**
- Modify: `crates/push-auth-core/src/rules.rs` (`handle`, ~lines 55–70; test `handles_drop_the_at_sign_and_case`)

- [ ] **Step 1: Write the failing test.** Replace the test body:

```rust
    #[test]
    fn handles_drop_the_at_sign_and_case() {
        assert_eq!(handle("@Alice_99"), Ok("alice_99".to_owned()));
        assert_eq!(handle("bob"), Ok("bob".to_owned()));
        for good in ["a-b", "has-dash", "@Yury-Opolev", "build-bot", "a-1_b-c"] {
            assert!(handle(good).is_ok(), "{good:?}");
        }
        assert_eq!(handle("@Yury-Opolev"), Ok("yury-opolev".to_owned()));
        for bad in [
            "al",
            "9lives",
            "_bob",
            "émile",
            "bob-",
            "a--b",
            "-bob",
            "a-",
            "has.dot",
            &"a".repeat(31),
        ] {
            assert_eq!(
                handle(bad),
                Err(invalid(
                    "handle",
                    "use 3 to 30 letters, digits, underscores or single hyphens, \
                     starting with a letter and not ending with a hyphen",
                )),
                "{bad:?}"
            );
        }
        assert!(handle(&"a".repeat(30)).is_ok());
        assert!(handle(&format!("{}-b", "a".repeat(28))).is_ok());
        assert!(handle(&format!("{}-b", "a".repeat(29))).is_err());
    }
```

- [ ] **Step 2: Run** `cargo test -p push-auth-core --lib rules -j 4`. Expected: FAIL (`a-b` refused).

- [ ] **Step 3: Implement.** Replace `handle`:

```rust
/// A public handle, without its `@`: 3 to 30 lower-case letters, digits, underscores and
/// hyphens, starting with a letter, not ending with a hyphen and without two hyphens in a
/// row. Input may carry the `@` and any case. Migration 0007's CHECK says the same.
pub fn handle(raw: &str) -> Result<String, Invalid> {
    let raw = raw.trim();
    let handle = raw.strip_prefix('@').unwrap_or(raw).to_lowercase();
    let mut chars = handle.chars();
    let starts_with_letter = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    let rest_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    let hyphens_ok = !handle.ends_with('-') && !handle.contains("--");
    if !(3..=30).contains(&handle.len()) || !starts_with_letter || !rest_ok || !hyphens_ok {
        return Err(invalid(
            "handle",
            "use 3 to 30 letters, digits, underscores or single hyphens, \
             starting with a letter and not ending with a hyphen",
        ));
    }
    Ok(handle)
}
```

- [ ] **Step 4: Run** the unit command. Expected: PASS. Then `cargo fmt --all`.

- [ ] **Step 5: Commit** `feat(auth): accept hyphens in handles` (rules.rs only).

### Task 2: Migration 0007 and the schema tests

**Files:**
- Create: `crates/push-auth-core/migrations/0007_hyphenated_handles.sql`
- Modify: `crates/push-auth-core/tests/schema.rs` (`malformed_handles_and_platforms_are_refused`; new test at the end)

- [ ] **Step 1: Write the failing tests.** In `malformed_handles_and_platforms_are_refused`,
the refused list becomes:

```rust
    for handle in [
        "al",
        "9lives",
        "bob-",
        "a--b",
        "-bob",
        "Alice",
        "a_very_long_handle_over_thirty_c",
    ] {
```

and append at the end of the file:

```rust
/// Migration 0007 widens the handle CHECK to the rule of `rules::handle`: hyphens, but not
/// at the end and never two in a row. Handles stored before it stay valid.
#[tokio::test]
async fn migration_0007_admits_single_hyphens_and_keeps_existing_handles() {
    let (_container, url) = auth_database().await;
    let before = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|migration| migration.version <= 6)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    let pool = PgPool::connect(&url).await.expect("connect");
    before.run(&pool).await.expect("migrations 1 to 6");
    let longest = "a".repeat(30);
    for handle in ["abc", "alice_99", longest.as_str()] {
        insert_user(&pool, &format!("{handle}@example.com"), handle)
            .await
            .expect(handle);
    }

    migrate_database(&url).await.expect("migration 0007");
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM auth.user_account")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(kept, 3);
    let hyphenated = format!("{}-b", "a".repeat(28));
    for handle in ["a-b", "has-dash", "yury-opolev", "a-1_b-c", hyphenated.as_str()] {
        insert_user(&pool, &format!("{}@example.com", Uuid::now_v7()), handle)
            .await
            .expect(handle);
    }
    let too_long = format!("{}-b", "a".repeat(29));
    for handle in ["bob-", "a--b", "-bob", "a-", "ab", "_ab", too_long.as_str()] {
        let error = insert_user(&pool, &format!("{}@example.com", Uuid::now_v7()), handle)
            .await
            .expect_err(handle);
        assert_eq!(constraint(error), "user_account_handle_check", "{handle}");
    }
}
```

- [ ] **Step 2: Run** the schema command. Expected: the new test FAILS (`a-b` violates the
CHECK); `malformed_handles…` passes already (they are refused by the old CHECK too).

- [ ] **Step 3: Implement** `0007_hyphenated_handles.sql`:

```sql
-- Handles may contain hyphens (REQUEST PUSH 42): 3 to 30 characters, a letter first, then
-- letters, digits, underscores or hyphens, never a hyphen last and never two in a row.
-- The same rule as push_auth_core::rules::handle. PostgreSQL's regular expressions have no
-- lookahead, so "no --" is a second condition. Every existing handle matches, so re-adding
-- the CHECK (which validates the rows) changes no data.
ALTER TABLE auth.user_account DROP CONSTRAINT user_account_handle_check;
ALTER TABLE auth.user_account ADD CONSTRAINT user_account_handle_check CHECK (
    handle::text ~ '^[a-z][a-z0-9_-]{1,28}[a-z0-9_]$' AND strpos(handle::text, '--') = 0);
```

- [ ] **Step 4: Run** the schema command. Expected: all schema tests PASS (including
`the_owner_migrates_and_a_second_run_changes_nothing`, which counts migrations).

- [ ] **Step 5: Commit** `feat(auth): widen the handle CHECK for hyphens (migration 0007)`.

### Task 3: End-to-end through the API

**Files:**
- Modify: `crates/push-auth-core/tests/api.rs` (new test after `a_user_changes_their_handle_and_the_old_one_is_free_at_once`)
- Modify: `crates/push-auth-core/src/tests/agents.rs` (new test at the end)

- [ ] **Step 1: Write the tests.** In `tests/api.rs` (helpers `with_device`, `call`,
`patch_me`, `lookup_status` exist):

```rust
#[tokio::test]
async fn a_hyphenated_handle_signs_up_and_is_found_ignoring_case() {
    let world = world().await;
    let yury = with_device(&world, "yury-opolev").await;
    let access = yury["access_token"].as_str().unwrap();
    let found = call(
        &world,
        Method::GET,
        "/v1/users/lookup?handle=%40Yury-Opolev",
        Some(access),
        None,
    )
    .await;
    assert_eq!(found.status, StatusCode::OK, "{}", found.text);
    assert_eq!(found.json()["user_id"], yury["user_id"]);
    assert_eq!(found.json()["handle"], "yury-opolev");

    let renamed = patch_me(&world, access, json!({"handle": "yury-o_1"})).await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.text);
    assert_eq!(renamed.json()["handle"], "yury-o_1");
    for bad in ["yury-", "yury--o", "-yury"] {
        let answer = patch_me(&world, access, json!({"handle": bad})).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{bad}");
    }
}
```

In `src/tests/agents.rs` (helpers `setup`, `call`, `caller` exist):

```rust
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
    for bad in ["deploy-", "deploy--bot"] {
        assert!(matches!(
            auth.change_agent(caller(&owner), user_id, Some(bad), None, false)
                .await,
            Err(AuthError::Invalid(_))
        ));
    }
}
```

- [ ] **Step 2: Run** the API commands. Expected: PASS (Tasks 1–2 already implement the
behaviour; these are the acceptance cases). Temporarily reverting `rules.rs` must make them
fail — check once, then restore.

- [ ] **Step 3: Commit** `test(auth): sign up, look up and reserve hyphenated handles`.

### Task 4: Release notes and docs

**Files:**
- Create: `CHANGELOG.md`
- Modify: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` (§3.1 implementation notes, ~line 138)
- Modify: `docs/superpowers/specs/2026-10-23-hyphenated-handles-design.md` (status line)

- [ ] **Step 1:** `CHANGELOG.md`:

```markdown
# Changelog

Notable changes for consumers of push-messenger. Newest first.

## Unreleased

### Changed

- **Handles may contain hyphens** (REQUEST PUSH 42). A handle is now 3 to 30 characters
  of `a-z`, `0-9`, `_` and `-`, starting with a letter, not ending with `-` and without
  `--` (so `yury-opolev` and `build-bot` are accepted; `bob-`, `a--b` and `-bob` are not).
  Input is still trimmed, lower-cased and stripped of one leading `@`; lookups and
  uniqueness still ignore case. This applies to signup, Entra and external sign-in,
  `PATCH /v1/users/me`, `GET /v1/users/lookup`, the admin API and
  `POST`/`PATCH /v1/agents`. The 400 reason is now "use 3 to 30 letters, digits,
  underscores or single hyphens, starting with a letter and not ending with a hyphen".
  **Update client-side handle validation** to match, e.g.
  `^[a-z][a-z0-9_-]{1,28}[a-z0-9_]$` plus "no `--`".
- auth migration `0007_hyphenated_handles.sql` replaces the `auth.user_account` handle
  CHECK. It only widens the rule; no data changes. It runs with the usual auth
  migrations.
```

- [ ] **Step 2:** in the design spec replace "Handles are stored in lower case: 3–30
letters, digits and underscores, starting with a\n  letter (a `CHECK`)." with "Handles are
stored in lower case: 3–30 letters, digits, underscores and\n  hyphens, starting with a
letter, never ending with a hyphen or holding two in a row (a\n  `CHECK`; hyphens since
migration 0007)." — rewrap to the file's ~90-column width.

- [ ] **Step 3:** set the new spec's status to "done ([plan](…))" (the PR number is added
after merge if known).

- [ ] **Step 4: Commit** `docs: release notes for hyphenated handles`.

### Task 5: Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`
- [ ] `cargo test -p push-auth-core -j 4` (all green, Docker running)
- [ ] `cargo +1.85 build -p push-auth-core --locked -j 4` if the 1.85 toolchain is installed
