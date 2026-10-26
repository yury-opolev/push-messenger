# Changelog

Notable changes for consumers of push-messenger. Newest first.

## Unreleased

### Added

- **Rich message templates and a template marketplace** (REQUEST PUSH feature 8). Two new
  content types: `CONTENT_TYPE_RICH` (3), a card filled from an approved tpl/1 template
  (`{tpl, params, fallback}`, up to 32 KiB), and `CONTENT_TYPE_ANSWER` (4), an answer to a
  card (`{card, values, fallback}`, up to 4 KiB). message-svc enforces everything, since
  bodies are not end-to-end encrypted: the template's approval, the full params schema,
  the sender's kind (`agent` for owned agents, `human` otherwise) and every answer. The
  first valid answer closes the card: the new `Deliver.answer_seq` carries it, the
  answer's `Deliver.target_seq` is the card's seq, and later answers get
  `ERROR_CODE_REJECTED`; answered cards and answers cannot be edited. Refused bodies are
  `ERROR_CODE_INVALID`; no new frame or error code. Guide:
  [docs/rich-messages.md](docs/rich-messages.md) (tpl/1 reference, review checklist,
  catalog protocol).
- auth-svc: `POST /v1/templates` submits a definition for review (access token, JSON up
  to 16 KiB; publishers are claimed by the first submitter, `agent`, `push`, `system`,
  `admin`, `builtin`, `official`, `verified` and every publisher starting with one of
  them are reserved, at most 20 pending per account);
  `GET /v1/templates?since=N` serves the client catalog of approved and revoked versions
  (pages of 200, `seq`/`more`, ETag `"tpl-<since>-<seq>"` with 304 on `If-None-Match`,
  `Cache-Control: private, no-cache`). The admin listener gains `GET /v1/admin/templates`,
  `GET /v1/admin/templates/{ref}` (definition, outline, audit trail) and
  `POST /v1/admin/templates/{ref}/approve|reject|revoke` (`{reviewer, note?}`): holders of
  `PUSH_AUTH_ADMIN_TOKEN` are the reviewers. New error codes `not_found` (404) and
  `conflict` (409); new rate limits: submissions 10 per hour per user and 60 per IP, the
  catalog 120 per minute per user and 600 per IP. New security events
  `auth.template.submitted`, `auth.template.refused` and `auth.admin.template_reviewed`
  (with the reviewer's IP); message-svc logs `templates.submitted`, `templates.approved`,
  `templates.rejected`, `templates.revoked`, `templates.seeded` and
  `templates.builtin_mismatch`.
- Built-in templates `agent.reply@1`, `agent.approval@1`, `agent.question@1` and
  `agent.plan@1` (agents only), seeded as approved when message-svc starts, before it is
  ready; an admin's revocation survives restarts. Designed upstream without the
  consumer's spec: reconcile and propose changes as new versions.
- messaging migrations `0005_templates.sql` and `0006_validate_message_checks.sql`:
  content types 3 and 4, `answer_seq` on messages, the registry (`template_publisher`,
  `template_version`, the `template_change` counter) and the append-only audit trail
  `messaging.template_event`, guarded by triggers (immutable versions, one-way
  statuses). They change no existing data and run with the usual messaging migrations.
  **Upgrade notes:**
  - 0005 is metadata-only: the new `answer_seq` column is nullable without a default and
    the new message CHECKs are added `NOT VALID`, so it holds the message table's
    exclusive lock only for a moment, without a scan (with `lock_timeout` 5 s: behind a
    long transaction it fails and start-up retries it instead of stalling all traffic).
    0006 then validates the CHECKs online: `VALIDATE CONSTRAINT` scans under SHARE UPDATE
    EXCLUSIVE, so reads and writes continue.
  - During a rolling upgrade old message-svc pods cannot decode content types 3 and 4: a
    request that reads a card or an answer fails there. Finish rolling out message-svc
    before approving or sending templates (the built-ins are approved as soon as the
    first new pod starts, so also before agents send cards).
  - Rolling back message-svc after 0005 is not supported: sqlx refuses a database with
    applied migrations the binary does not know (`VersionMissing`), so an older
    message-svc that migrates at start-up (`PUSH_RUN_MIGRATIONS=true`, the default) keeps
    retrying and never becomes ready, and an older `push-message-svc migrate` job fails.
    With `PUSH_RUN_MIGRATIONS=false` an older binary starts but fails every request that
    reads a card or answer.
  - Older SDKs do not know content types 3 and 4: they show RICH and ANSWER messages as
    text, their raw JSON body, and keep them as text after an SDK upgrade (entries
    already received are not reclassified). Ship the updated SDK to your apps before
    agents send cards.
- message-svc serves a new internal gRPC service, `push.internal.v1.Templates`, on the
  Directory port with auth-svc's existing token: **no new configuration** (auth-svc uses
  `PUSH_MESSAGE_SVC_URL` and `PUSH_DIRECTORY_TOKEN`; without them the template routes
  answer 503). The chart's auth Ingress routes `/v1/templates`.
- SDK and bindings: `MessageKind::Rich`/`Answer` (`rich`/`answer`), `answerSeq` on cards
  and `cardSeq` on answers, and `send_rich`/`send_answer` (Node `sendRich`/`sendAnswer`,
  C `push_client_send_rich`/`push_client_send_answer`) with local envelope checks. The
  SDK does not fetch the catalog yet.
- **Agent-key sign-in for owned agents** (REQUEST PUSH feature 6). A new opt-in provider,
  `agent-key` in `PUSH_AUTH_PROVIDERS` (off by default), lets an owned agent sign in with
  a P-256 key pair of its own instead of an Entra app token: no secret, certificate or
  managed identity. The agent sends a single-use ES256 JWS (`typ: push-agent+jwt`, its
  public key in the `jwk` header, `aud`/`iat`/`exp`/`jti` claims, at most 300 s of
  lifetime) to `POST /v1/auth/agent-key`; the first one, with the owner's enrollment code
  (`{token, enrollment_code}`), binds the key's RFC 7638 thumbprint to the reserved agent.
  Then `{token}` and `{token, device_id}` sign in as for `entra-app`. An unenrolled key
  gets 401 `token_invalid` and never creates an account; a reused `jti` is refused across
  replicas. New variables `PUSH_AUTH_AGENT_KEY_AUDIENCE` (default: the public URL without
  a trailing `/`) and `PUSH_AUTH_AGENT_KEY_MAX_SESSION_HOURS` (1 to 720, default 24); new
  events `auth.agent-key.success` and `auth.agent-key.failure`. `POST /v1/agents` now
  also works with `agent-key` alone (the reservation has no tenant, and only a key can
  redeem it). Key rotation is not supported yet: delete the agent and enroll a new one.
  Guide: [docs/agent-key-sign-in.md](docs/agent-key-sign-in.md), with a PowerShell
  example that keeps the key in the TPM.
- `GET /v1/agents` lists two more fields per agent: `credential` (`"entra-app"`,
  `"agent-key"`, or `null` while pending) and `key_thumbprint` (the key's thumbprint for
  `agent-key`, otherwise `null`).
- Helm: `auth.signIn.providers` accepts `agent-key`, and
  `auth.signIn.agentKey.{audience,maxSessionHours}` render the two variables (the
  audience only when set). The chart validates both.
- auth migration `0009_agent_key.sql` makes `auth.agent_enrollment.tenant_id` nullable
  and adds the `auth.agent_key_assertion` replay table. It changes no existing data and
  runs with the usual auth migrations. During a rolling upgrade an old pod fails (500)
  only a redemption of a tenantless reservation, which only a new pod with `agent-key`
  makes.

### Changed

- `serve_with` refuses an `Overrides` authenticator with the built-in id `agent-key`, as
  for `entra-app`. An `Overrides` admission is also accepted with `agent-key` alone, and
  the start-up error without any sign-in by token now names all of them (`entra`,
  `entra-app`, `agent-key`, `external`).
- Chart: the email errors now say "without password" instead of naming `entra`, since
  any `auth.signIn.providers` list without `password` writes no email.
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