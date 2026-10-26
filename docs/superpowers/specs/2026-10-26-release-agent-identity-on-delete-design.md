# Release an agent's identity when the agent is deleted — design

Request: REQUEST PUSH feature 7 (consumer `yuopolev/push-messenger`, against main `d36e398`).
Supersedes the sentence "Disabled bindings are permanent tombstones" of
`2026-10-19-entra-sign-in-design.md` D5 and the matching paragraph of `docs/entra-sign-in.md`.

## Problem

`DELETE /v1/agents/{id}` of an **active** agent sets `auth.user_account.disabled_at` and
revokes its devices, but keeps its `auth.external_identity` row
(`provider = 'entra-app'`, `issuer = tenant`, `subject = service principal oid`). Because
`(provider, issuer, subject)` is unique and redemption refuses any existing row with
`identity_in_use`, that app registration or managed identity can never enroll again.

## Decisions

| # | Decision |
|---|---|
| R1 | Deleting an active agent **releases every external identity of the agent account**, whatever its provider, in the same transaction that sets `disabled_at` and revokes devices and refresh tokens. Provider-agnostic on purpose: the planned `agent-key` provider (REQUEST PUSH feature 6) reuses it unchanged. |
| R2 | Release = the identity row is moved, not detached: it is deleted from `auth.external_identity` and copied, with the release time, into a new append-only table `auth.released_identity` (migration `0008_released_identity.sql`). The live table keeps its meaning ("this identity signs in to this account") and its unique key keeps protecting live bindings. |
| R3 | Auditing. `auth.released_identity (identity_id PK, provider, issuer, subject, user_id → user_account, created_at, released_at)` keeps who was bound, since when and until when; the enrollment facts (`tenant_id`, `client_app_id`, `consent_device_id`, `enrolled_at`) stay in `auth.agent_enrollment`, joined by `user_id`. One structured `security` log line per released identity: `event = "auth.agent.identity_released"`, with `user_id`, `owner_user_id`, `provider`, `issuer`, `subject`, `client_app_id`. The existing route-level `auth.agent.deleted` event is unchanged. Not unique on `(provider, issuer, subject)`: one app may be enrolled and released many times. |
| R4 | A deleted agent stays unusable by every path: devices and refresh tokens are revoked (unchanged); access tokens fail because the account is disabled (unchanged); old registration tokens name the released `identity_id`, which no longer exists, so registration answers 401; a sign-in of the released app finds no identity, so it gets the **normal unenrolled answer** (`entra-app` is linked-only: 403 `sign_in_not_permitted`, no account created) — it never resolves to the disabled account. |
| R5 | Re-enrollment: `POST /v1/auth/entra-app {token, enrollment_code}` with the released app and a fresh reservation (any owner, any allowed tenant rules as today) succeeds and binds the app to the new agent. The deleted agent remains disabled. |
| R6 | `identity_in_use` is unchanged: two live (or pending-redemption) agents never share one app; the unique key stays the final authority. A redemption racing an uncommitted deletion of the same app may still answer `identity_in_use`; retrying after the `DELETE` returns succeeds. |
| R7 | Other disable/delete paths. There is exactly one path that disables an account: owner `DELETE /v1/agents/{id}` of an active agent (`Auth::change_agent`). Deleting a **pending** agent, or its expiry, hard-deletes an account that never had an identity — nothing to release. The admin API only revokes sessions (`POST /v1/admin/users/revoke-sessions`); that is sign-out, not deletion, and keeps the identity so the agent can sign in again. There is no human-account deletion: `owner_user_id` has no `ON DELETE`, so an owner with agents cannot be removed, and no cascade exists to handle. Should either be added later, it must call the same release helper. |
| R8 | Legacy linked-only app accounts (`kind = 'user'` with an `entra-app` identity) are not agents; nothing deletes them, and their identity is untouched. |

## Components

- `crates/push-auth-core/migrations/0008_released_identity.sql`: the table, an index on
  `(provider, issuer, subject)` for audit lookups and one on `user_id`.
- `crates/push-auth-core/src/agents.rs`: `release_identities(&mut PgConnection, agent, owner)`
  — one `WITH gone AS (DELETE … RETURNING …) INSERT INTO auth.released_identity … RETURNING`
  statement, then the log line per row. Called by `change_agent` after `disabled_at` is set
  and devices are revoked, inside the same transaction. Module docs updated.
- Docs: `docs/entra-sign-in.md` (DELETE row, tombstone paragraph, step 6),
  `CHANGELOG.md` Unreleased (behaviour change + migration).

## Locking

Deletion keeps its order (owner row → agent row → device rows) and then takes the identity
rows' locks. Sign-in's `UPDATE auth.external_identity … last_sign_in_at` is a single
autocommit statement holding no other lock; redemption inserts the identity after its own
owner/agent locks and only waits on the deleted key. No cycle is introduced.

## Testing (Docker/testcontainers)

1. Enroll app A as agent X, delete X, reserve Y, redeem with A → 200, Y active, X disabled;
   X's old access/refresh/registration tokens refused; A without a code signs in to Y.
   Before Y exists, A without a code → 403 `sign_in_not_permitted` and no account created.
   `auth.released_identity` holds A's row for X with `released_at`.
2. Two live agents still cannot share A (`identity_in_use`) — existing concurrency test,
   plus: after X is deleted and A bound to Y, a third reservation redeemed with A → 409.
3. Existing deletion/race tests updated: the old code of a deleted agent is spent
   (`enrollment_invalid`), no longer `identity_in_use`.
4. Schema test lists the new table.
