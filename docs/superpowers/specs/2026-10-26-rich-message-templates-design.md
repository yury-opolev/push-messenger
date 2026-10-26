# Rich message templates and a template marketplace — design

REQUEST PUSH feature 8. Agents need structured content (thinking, tool steps) and
questions answered with a click (approval and choice cards). Until now a message is
`TEXT` or `SYSTEM_EVENT` only, so every client ships its own layouts and nothing stops a
person, or a modified client, from sending rich content that imitates an agent.

This design adds reviewed, server-enforced templates: a registry with an admin review
flow, two content types (`RICH`, `ANSWER`), enforcement on send and edit, server-side card
state, and four built-in `agent.*` templates. The consumer's spec
(`docs/specs/2026-10-26-rich-messages.md` in the consumer repository) was not available
upstream; tpl/1 below is designed here from the change request and documented in
[docs/rich-messages.md](../../rich-messages.md) so the consumer can reconcile.

## Decisions

**D1 — The server sees message bodies, so it enforces everything.** push-messenger does
not encrypt message bodies end to end (spec 2026-10-11, "out of scope"): message-svc
stores and serves `body` as plaintext bytes behind TLS. So the server validates the whole
RICH body (template, params, sender kind) and the whole ANSWER body (card, choice), not
just metadata. Should end-to-end encryption arrive later, enforcement must move to a
cleartext envelope (template ref, sender kind, card reference); the docs say so.

**D2 — The registry lives in message-svc's database.** Each service owns its own
database (push_auth, push_messaging, push_worker); message-svc enforces templates on every
send, so the registry is there, next to the messages: a revocation is seen by the very
next send, in the same database, without replication. New migration
`push-store/migrations/0005_templates.sql`.

**D3 — One validator, shared.** tpl/1 parsing, the meta-rules, body validation and the
built-in definitions are pure code in `push-domain::templates` (no I/O), used by
message-svc and by `push-sdk` (client-side checks before queueing).

**D4 — auth-svc fronts HTTP; message-svc answers over a new internal gRPC service.**
auth-svc already serves the client HTTP API (`/v1/...`) and the operator admin listener,
and already calls message-svc's `Directory` with its own internal token. A new internal
service `push.internal.v1.Templates` on message-svc (Submit, Catalog, List, Get, Review) is
guarded by the same auth-svc token (`PUSH_DIRECTORY_TOKEN`: one token per caller, and the
caller is the same), so the chart needs no new secret.

**D5 — Admins are the operator admin role.** Review is served only on auth-svc's admin
listener (`PUSH_AUTH_ADMIN_ADDR`, `PUSH_AUTH_ADMIN_TOKEN`, never routed by the Ingress,
spec D15). Holders of the admin token are the template reviewers. Each review call names
its `reviewer` (1 to 64 printable characters), which goes into the audit trail with the
caller's IP. A per-user reviewer group on the public API was rejected: it would put an
approval endpoint on the internet-facing listener for little gain over the isolated one.

**D6 — Publishers claim a namespace.** Any signed-in account may submit
(`POST /v1/templates`). The first accepted submission under a publisher id claims it for
the submitting account; later versions and names under it must come from the same
account. `agent`, `push`, `system`, `admin` and `builtin` are reserved (the server owns
`agent`). At most 20 pending submissions per account; submissions are rate limited.

**D7 — Immutable versions, a one-way status.** A ref is `<publisher>.<name>@<version>`.
Statuses: `pending → approved | rejected`, `approved → revoked`; `rejected` and `revoked`
are final. The definition of a stored version never changes (a trigger refuses it); a
change is a new version. A refused ref cannot be resubmitted.

**D8 — A change sequence drives the client catalog.** Every approval and revocation
(and the seeding of a built-in) takes the next value of a single-row counter under its
row lock, so change numbers commit in order and a client that has seen `N` never misses a
change below `N`. `GET /v1/templates?since=N` returns approved and revoked versions
changed after `N` (oldest first, at most 200 per page) and `seq`, the position to ask from
next. Its ETag is `"tpl-<since>-<seq>"`; `If-None-Match` answers 304.

**D9 — Sender kind is derived, never claimed.** `agent` when the sender has a row in
`messaging.agent_owner` (an owned agent, enrolled through auth, PR #46/#47), otherwise
`human`. Legacy linked-only app accounts, which predate owned agents and are not known to
messaging as agents, count as human: they can send templates open to humans, not
agent-only ones. Misclassifying never lets a person send an agent-only template.

**D10 — Card state is server-side (item 4 implemented).** An ANSWER references a card
(a RICH message with an interactive block). The first valid answer marks the card
answered (`answer_seq`, the answer's seq); later answers are refused (`REJECTED`, "the
card is already answered"). The answer entry fans out like any message, carries
`target_seq` = the card's seq, and `Deliver.answer_seq` shows the state on the card in
history, so senders never edit cards to close them.

**D11 — Revocation.** A revoked version is refused for new sends, edits and answers; the
catalog lists it as `revoked`, and clients stop rendering it, in new and existing
messages, showing the message's `fallback` text instead. Stored bodies are kept.

**D12 — Built-ins.** `agent.reply@1`, `agent.approval@1`, `agent.question@1` and
`agent.plan@1` ship in `push-domain` (JSON files, validated by the same code) and are
seeded at message-svc start-up as approved, owned by the server. Seeding inserts missing
rows only, so an admin's revocation of a built-in survives restarts.

**D13 — Audit.** Every submit, approve, reject, revoke and seed is a row in the
append-only `messaging.template_event` (ref, action, actor account or reviewer label,
note, time) and a `security` log event (`templates.submitted`, `templates.approved`,
`templates.rejected`, `templates.revoked`, `templates.seeded`). Refused sends are logged
at debug only (no body content is ever logged).

## Wire changes (push-proto)

`frame.proto`: `ContentType` gains `CONTENT_TYPE_RICH = 3` and `CONTENT_TYPE_ANSWER = 4`.
`Deliver` gains `optional int64 answer_seq = 19` (cards only). `Deliver.target_seq` is
also set on ANSWER messages (the card's seq). The reserved `ephemeral`/`urgent`/
`attachments` fields are untouched; no new frame or error code: a refused body is
`ERROR_CODE_INVALID`, a closed card `ERROR_CODE_REJECTED`, a missing card
`ERROR_CODE_TARGET_NOT_FOUND`.

`messaging.proto`: new service `Templates` (internal, auth-svc's token).

## tpl/1 (summary; the full format is in docs/rich-messages.md)

A definition is a JSON object, at most 16 KiB, parsed strictly (unknown fields refused,
duplicate keys refused):

```json
{
  "format": "tpl/1",
  "publisher": "agent", "name": "approval", "version": 1,
  "title": "Approval request",
  "senders": ["agent"],
  "params": {
    "title":   {"type": "string", "max_len": 120},
    "details": {"type": "string", "max_len": 4000, "multiline": true},
    "preview": {"type": "string", "max_len": 4000, "multiline": true, "optional": true}
  },
  "layout": [
    {"type": "heading", "param": "title"},
    {"type": "text", "param": "details"},
    {"type": "preview", "param": "preview", "label": "Preview"},
    {"type": "buttons", "options": [
      {"id": "approve", "label": "Approve", "style": "primary"},
      {"id": "reject", "label": "Reject", "style": "danger"}]}
  ]
}
```

- Ids (`publisher`, `name`): `^[a-z][a-z0-9-]{0,31}$`. Param and option ids:
  `^[a-z][a-z0-9_]{0,31}$`. `version`: 1 to 65535.
- Param types: `string` (`max_len` 1-4000, `multiline`), `enum` (1-16 ids), `int`
  (`min`/`max` within ±2^53), `bool`, `list` (`item`: `string` with `max_len`, `option`
  = `{id, label}`, or `step` = `{label, status, detail?}`; `max_items` 1-50). Any param
  may be `optional`. At most 16 params; every param must be used by a block.
- Blocks (at most 12): `heading`, `text` (literal `text` or `param`), `preview`,
  `thinking` (a string param), `steps` (a `list` of `step`), `buttons` (1-4 literal
  options), `single` and `multi` (1-10 literal options or a `list` of `option` param;
  `multi` has `min`/`max`), `status` (an `enum` param). At most one interactive block
  (`buttons`, `single`, `multi`). A block whose optional param is absent is skipped.
- Literal wording (`title`, `text`, `label`): at most 500 (`text`) or 80 characters;
  no control characters (a `text` block may hold line breaks), no invisible or bidi
  formatting characters, no markup (`<`, `>`, `` ` ``, `](`, `{`, `}`), and nothing
  URL-like: a scheme (`x://`, `javascript:`, `data:`, `vbscript:`, `file:`, `mailto:`,
  `tel:`, `sms:`, `blob:`), `www.`, `//`, or a dotted host with a common top-level
  domain.

RICH body (at most 32 KiB): `{"tpl": "agent.approval@1", "params": {...}, "fallback":
"..."}`; `fallback` is 1-2000 characters of plain text for clients that cannot render
it. ANSWER body (at most 4 KiB): `{"card": "<message uuid>", "values": ["approve"],
"fallback": "Approved"}`.

## Enforcement (message-svc)

On Send (`content_type` TEXT, RICH or ANSWER; anything else is `INVALID`):

- RICH: the body parses; the ref names a stored version whose status is `approved`; the
  params match its schema (types, caps, required, no extras, strings without control or
  invisible formatting characters); the sender's kind is in `senders`.
- ANSWER: the body parses; the card is a message of the same conversation the answerer
  may see, RICH, not deleted, not sent by the answerer, not answered yet; its template is
  still approved and has an interactive block; `values` are ids it offers (from literal
  options or the card's own `list` param), exactly one for `buttons`/`single`, `min` to
  `max` for `multi`.

On Edit, the body is checked as the target's content type: a TEXT edit is text, a RICH
edit is a full RICH body checked as above (any approved template the sender may use), an
ANSWER cannot be edited, and an answered card cannot be edited (`REJECTED`). Delete is
unchanged (deleting an answer does not reopen its card).

## HTTP (auth-svc)

Public, with an access token: `POST /v1/templates` (submit; 201 with the summary),
`GET /v1/templates?since=N` (catalog, ETag). Admin listener, with the admin token:
`GET /v1/admin/templates?status=pending|approved|rejected|revoked` (list),
`GET /v1/admin/templates/{ref}` (definition, a plain-text outline as the preview, the
audit trail), `POST /v1/admin/templates/{ref}/approve|reject|revoke` with
`{"reviewer": "...", "note": "..."}`. Errors: 400 `invalid` (with the meta-rule that
failed), 404 `not_found`, 409 `conflict` (exists, wrong status, publisher of another
account, too many pending), 503 when message-svc does not answer.

## Clients and the SDK

The SDK maps content types 3 and 4 to `MessageKind::Rich` and `MessageKind::Answer`
(older SDKs stored any unknown type as text), keeps `answer_seq` on cards and applies
incoming answers to their card, and offers `send_rich` and `send_answer` with the shared
envelope checks. The bindings expose the new kinds and the two calls. Clients must render
only refs in their catalog with status approved, render blocks as inert text (never
linkify, never interpret markup), show the sender, and fall back to `fallback` for
unknown or revoked refs.

## Testing

- `push-domain` unit tests: every meta-rule (URL, unknown block, unknown field, cap and
  cap + 1, unused param, two interactive blocks), param validation, answer validation,
  the built-ins parse.
- Testkit scenarios on the in-memory store and the Postgres store: send valid RICH,
  refused unapproved / revoked / invalid / disallowed sender kind; answers (valid, wrong
  conversation, second answer, own card, bad value); edits.
- push-store tests: the migration's immutability trigger and status transitions, the
  counter.
- message-svc gRPC tests for `Templates`; auth-svc tests for the HTTP routes and ETag.
- push-smoke whole-stack test: submit, approve, fetch, send valid (200 path) and an
  unapproved send refused.

## Out of scope / follow-ups

- Catalog fetch in the SDK (clients call `GET /v1/templates` themselves for now).
- Per-user reviewer accounts on the public API; localisation of literal wording.
- End-to-end encryption (would move enforcement to a cleartext envelope, D1).
