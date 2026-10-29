# Changelog

Notable changes for consumers of push-messenger. Newest first.

## Unreleased

### Added

- **Safe Markdown in templates**: a tpl/1 `markdown` block (`{"type": "markdown", "param":
  "<multiline string>"}`, no literal, no label) whose value the server checks against a
  CommonMark subset: paragraphs, emphasis, strikethrough, code spans, fenced and indented
  code, headings 1–6, lists nested up to 4 and quotes up to 3 levels, GFM tables up to 20
  columns and 200 rows, thematic breaks, and links and autolinks to absolute http(s) URLs
  only. Raw HTML, images, footnotes and any other link scheme (`javascript:`, `data:`,
  `file:`, `mailto:`, …, also in link reference definitions) are refused with
  `ERROR_CODE_INVALID` and the reason. The subset and how clients must render it (from a
  Markdown AST into native elements, links with their URL shown and confirmed, no images
  or remote content, code never run) are in
  [docs/rich-messages.md](docs/rich-messages.md#markdown).
- **`agent.reply@2`** built-in: `agent.reply@1` with `text` rendered as Markdown. It is
  seeded at start like the other built-ins; `@1` stays. Clients that do not know it show
  the fallback.
- **Agent bridge**: `/yolo`, the same as `/mode bypass` (the same confirmation card,
  reset and `/status` line). The `Backend` trait declares its `TextFormat` (plain by
  default); Coda writes Markdown, so its turns and background messages are sent as
  `agent.reply@2` with the Markdown kept and masked, a marker-free fallback, and pages
  that never end inside a code block. The bridge's own messages stay `agent.reply@1`.

- **Agent bridge with a Coda backend**: `push-agent-bridge` (new crate `push-bridge`)
  reaches a local AI coding agent from push-messenger as a private agent contact. Guide:
  [docs/agent-bridge.md](docs/agent-bridge.md) (setup on Windows, commands, security
  model, troubleshooting, live smoke checklist).
  - A backend-independent core: agent-key sign-in with the OS key, the owner gate ("This
    agent is private." to anyone else, at most once per sender per hour), one
    `agent.reply@1` per turn (thinking, up to 50 steps, text paged at 4,000 characters,
    edits at most every 2 s, "(stopped)"), cards for the agent's requests with the
    owner's answers checked by the bridge and a 10-minute deadline ("No answer;
    skipped"), activity (working, waiting, idle), secret masking and caps on everything
    sent, the owner's `/` commands (`/new`, `/cd` inside configured roots, `/stop`,
    `/status`, `/mode bypass` behind a confirmation card, …), sessions per folder, and
    restarts with backoff (1 s to 60 s). A small `Backend` trait with typed events and
    capabilities; a `TestBackend` runs the core without Coda.
  - The Coda backend drives `coda serve` (JSON-RPC 2.0 over stdio): prompts, steering,
    permission, question and plan requests, background messages and task completions,
    and Coda's session, model, effort, mode, goal, task, schedule and skill commands. It
    never sets `CODA_SERVE_API_KEY`.
  - `config.json`, `state.json` (no secrets) and a JSON-lines log without bodies or
    tokens in `%LOCALAPPDATA%\push-messenger\coda-bridge` (or `--dir`); commands `keygen`,
    `thumbprint`, `enroll --code`, `run`, `status`; `scripts\install.ps1` and
    `uninstall.ps1` register and remove a logon Scheduled Task that restarts on failure.
- **push-sdk: agent-key sign-in through a signer**
  ([guide](docs/agent-key-sign-in.md#from-push-sdk-rust)).
  `Client::enroll_with_agent_key` and `Client::sign_in_with_agent_key` take an
  `AgentSigner` the app supplies, so the private key stays in OS key storage: the SDK
  builds the claims (`aud`, `iat`, `exp = iat + 120`, a new `jti` for every request) and
  the signer returns the compact JWS. Helpers: `AgentClaims`, `AgentPublicKey::thumbprint`
  (RFC 7638), `assemble_assertion`. A failing signer is the new `SdkError::Signer`, code
  `signer` (not retryable; the key provider's code, never key material). Rust only: the C
  and Node bindings do not expose it yet.
- **push-sdk: `CardBook`**, which checks answers to the cards an app sent: from the card's
  owner, in its conversation, naming the card, with offered values only, before its
  deadline (the server does not check who answers). Rust only.
- **`push-agent-key`** (new crate): agent-key signers. `cng::CngKey` keeps a persisted,
  non-exportable P-256 key in Windows key storage, in the TPM (Microsoft Platform Crypto
  Provider) when it can and otherwise in the Microsoft Software Key Storage Provider;
  `software::SoftwareKey` keeps the key in a PKCS#8 file, for tests and development only.
- **Built-in templates `agent.choose@1`** (multiple choice, 1 to 10 of up to 10 options)
  **and `agent.plan-approval@1`** (a plan of up to 4,000 characters with Approve plan and
  Reject), for owned agents ([definitions](docs/rich-messages.md#6-built-in-templates)).
  message-svc seeds them as approved at its next start; nothing else to do.
- **Live activity and presence** (REQUEST PUSH feature 12). Guide:
  [docs/activity-presence.md](docs/activity-presence.md) (frames, rules, multi-replica
  design, agent recipe).
  - Activity: a new client frame `Activity` (10) says what the user is doing in a
    conversation (`typing`, `working`, `waiting`, `idle`, with an optional plain-text label
    of 1 to 60 characters; empty means none). The other current members' connected devices
    hear it as `ActivityEvent` (42) with `ttl_ms` 30000 (0 for idle), to expire locally at
    receive time + ttl; the sender's own devices and members with a block either way do
    not. Never stored. Paced per device (burst 2, then 1 per second; extra frames are not
    refused, the latest per conversation is sent when a token frees up); a closed
    connection (not a takeover) sends idle for its live activities. Refusals are `Error`
    frames without ids that are never retryable.
  - Presence: `PresenceEvent` (43) pushes a contact's `ONLINE`/`OFFLINE` (with a whole
    `last_seen` minute) on change, and the request `GetPresence` (20, answered by
    `PresenceList` 11) reads 1 to 100 users, rate limited to 120 per minute per user.
    Visible only between accepted contacts; everything else (strangers, blocks, opt-outs,
    unknown ids) reads `UNKNOWN`, indistinguishably. Online while any connection on any
    gateway replica holds a lease in Redis (90 s, renewed every 30 s); a closed
    connection keeps it for a grace (`PUSH_PRESENCE_GRACE_MS`). Blocking an accepted
    contact pushes `UNKNOWN` both ways.
  - Opt-out: `SetPresenceSharing` (21, answered by `PresenceSharing` 12; without a value it
    reads the setting) switches sharing off or on; reciprocal (a user who hides sees
    everyone as `UNKNOWN`).
  - Internal: `Messaging.Activity` and `Messaging.AnnouncePresence` gRPC calls and
    `Notification.activity_changed`/`presence_changed`; Redis keys
    `presence:{<user>}:conns` and `presence:{<user>}:state`.
  - SDK and bindings: `send_activity`, `get_presence`, `set_presence_sharing`,
    `presence_sharing` (Node `sendActivity`, `getPresence`, `setPresenceSharing`,
    `presenceSharing`; C `push_client_send_activity`, `push_client_get_presence`,
    `push_client_set_presence_sharing`, `push_client_presence_sharing`) and the events
    `ActivityChanged`/`PresenceChanged` (`activityChanged`/`presenceChanged`, with
    millisecond fields `expiresAtMs` and `lastSeenMs`).
  - Helm: `gateway.presenceGraceMs` renders `PUSH_PRESENCE_GRACE_MS`.

  **Upgrade notes:**
  - messaging migration `0007_presence_hidden.sql` adds the table
    `messaging.presence_hidden` (the opt-out); it changes no existing data and runs with
    the usual messaging migrations. Rolling message-svc back after it is not supported,
    as for 0005.
  - Deploy message-svc before the gateways. A new gateway in front of an old message-svc
    answers `Activity` with `UNAVAILABLE`, gets `INVALID` for the presence requests and
    cannot announce presence; old gateways ignore the new notifications.
  - New gateway variable `PUSH_PRESENCE_GRACE_MS`: 0 to 120000 milliseconds, default
    10000. No new secret: presence uses the Redis the gateways and message-svc share.
  - Additive protocol: older SDKs log the new server frames as unreadable and ignore them.
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
    EXCLUSIVE, so reads and writes continue. The scan grows with the message table and
    runs before the pod reports ready: on a large installation run the migrations as a
    job first (`push-message-svc migrate`, the chart's migrate job) or allow a longer
    start-up probe.
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

- **Agent bridge**: a card ends the reply before it. When a request opens a card, the
  turn's reply gets its final state (a step still running shows as `pending`) and is not
  edited again; what the agent does after the owner's answer goes into a new reply below
  the answer, where the waiting step finishes. Requests in a row post their cards one
  after the other, and a turn that ends right after a card posts no empty reply. See
  [docs/agent-bridge.md](docs/agent-bridge.md#6-behaviour).
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