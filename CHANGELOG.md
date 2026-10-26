# Changelog

Notable changes for consumers of push-messenger. Newest first.

## Unreleased

### Changed

- **Deleting an agent releases its app identity** (REQUEST PUSH feature 7). `DELETE
  /v1/agents/{agent_id}` of an active agent still disables it for good and revokes its
  devices and tokens, and now also releases its app registration or managed identity in
  the same transaction: a fresh `POST /v1/auth/entra-app {token, enrollment_code}` with
  that app enrolls a new agent (of any owner) instead of failing with `identity_in_use`.
  Until then the released app gets the normal unenrolled 403 `sign_in_not_permitted`.
  Two live agents still cannot share an app. The released binding is kept in the new
  `auth.released_identity` table (tenant and client app id stay on
  `auth.agent_enrollment`) and logged as `auth.agent.identity_released`. Admin
  revoke-sessions is unchanged and does not release.
- auth migration `0008_released_identity.sql` adds the `auth.released_identity` audit
  table. It changes no existing data and runs with the usual auth migrations.
- **Handles may contain hyphens** (REQUEST PUSH 42). A handle is now 3 to 30 characters
  of `a-z`, `0-9`, `_` and `-`, starting with a letter, not ending with `-` and without
  `--` (so `yury-opolev` and `build-bot` are accepted; `bob-`, `a--b` and `-bob` are not).
  Input is still trimmed, lower-cased and stripped of one leading `@`; lookups and
  uniqueness still ignore case. This applies to signup, Entra and external sign-in,
  `PATCH /v1/users/me`, `GET /v1/users/lookup`, the admin API, `POST /v1/agents` and
  `PATCH /v1/agents/{agent_id}`. The 400 reason is now "use 3 to 30 letters, digits,
  underscores or single hyphens, starting with a letter and not ending with a hyphen".
  **Update client-side handle validation** to match, e.g.
  `^[a-z][a-z0-9_-]{1,28}[a-z0-9_]$` plus "no `--`".
- auth migration `0007_hyphenated_handles.sql` replaces the `auth.user_account` handle
  CHECK. It only widens the rule; no data changes. It runs with the usual auth
  migrations.