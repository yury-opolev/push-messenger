# Rich messages: templates, cards and answers

REQUEST PUSH feature 8. A message can be a **card** filled from a reviewed template
(`CONTENT_TYPE_RICH`) or an **answer** to a card (`CONTENT_TYPE_ANSWER`), next to plain
`TEXT`. Templates are published by anyone, reviewed by the operator's admins, and enforced
by the server on every send and edit. Design:
[spec 2026-10-26](superpowers/specs/2026-10-26-rich-message-templates-design.md)
(decisions D1 to D13). Where this guide and the spec differ, this guide describes what the
code does.

- [1. Overview](#1-overview)
- [2. Enforcement model](#2-enforcement-model)
- [3. Sender kinds](#3-sender-kinds)
- [4. tpl/1 reference](#4-tpl1-reference)
- [5. Message bodies and card state](#5-message-bodies-and-card-state)
- [6. Built-in templates](#6-built-in-templates)
- [7. Publishing templates](#7-publishing-templates)
- [8. Review (admins)](#8-review-admins)
- [9. Client catalog protocol](#9-client-catalog-protocol)
- [10. Audit](#10-audit)
- [11. Limits and follow-ups](#11-limits-and-follow-ups)

## 1. Overview

Agents produce structured content: a heading, their thinking, a list of tool steps, a
preview of a command, and questions a person answers with one click ("Approve" or
"Reject", pick one option, pick several). As plain text every client would invent its own
layout, and nothing would stop a person, or a modified client, from sending content that
looks like an agent's approval request.

So rich content is filled from **templates**:

- A template is a small JSON definition in the **tpl/1** format: typed params, a layout of
  blocks, and which kinds of sender may use it. Its literal wording is checked (no links,
  no markup, no invisible characters).
- Anyone signed in may **publish** a template version; it stays `pending` until an admin
  **approves** it. Approved versions form the **catalog** that clients fetch and render.
- A RICH message names an approved version (`agent.approval@1`) and fills its params. The
  server checks the version's status, every param and the sender's kind on every send and
  edit.
- A card with buttons or choices is answered with an ANSWER message. The server checks the
  answer against the card and closes the card with the first valid one.
- Four built-in templates (`agent.reply@1`, `agent.approval@1`, `agent.question@1`,
  `agent.plan@1`) are available from the start, for owned agents only.

## 2. Enforcement model

push-messenger does **not** encrypt message bodies end to end: message-svc stores and
serves `body` as plaintext bytes behind TLS (spec 2026-10-11, out of scope). So the server
can, and does, enforce everything a body says: that the template is approved, the full
params schema, the sender's kind, and every answer against its card. A client cannot get a
RICH or ANSWER message past the server that breaks these rules; clients still have their
own part, because only they render.

| The server enforces (message-svc) | Clients must |
|---|---|
| The body parses as strict JSON of its content type, within its size cap | Render only refs that are in their catalog with status `approved` |
| The ref names a stored version whose status is `approved` (missing, pending, rejected and revoked all get the same refusal) | Render blocks as inert text: never linkify, never interpret Markdown, HTML or any other markup in values |
| Every param: no unknown, every required one present, each of its type and within its caps, strings without control or invisible characters | Show who sent the message (the sender's name, and that it is an agent when it is one) |
| The sender's kind (`agent` or `human`) is one of the definition's `senders` | Show `fallback` for an unknown or revoked ref, or one they cannot render |
| An answer names a live card of the same conversation, from someone else, not answered yet, whose template is still approved, with values the card offers | Never render a `TEXT` message as a card, even if its text is a RICH body |
| The first valid answer closes the card (`answer_seq`); later ones are refused | Show a closed card as closed (`Deliver.answer_seq` set) and offer no buttons on it |
| Edits are checked as the target's content type; answered cards and answers cannot be edited | Stop rendering a version as soon as the catalog lists it `revoked`, in new and existing messages |

If end-to-end encryption is introduced later, the server can no longer read bodies.
Enforcement would then have to move to a cleartext envelope next to the ciphertext (the
template ref, the sender kind and the card reference), and the param checks would become
client-side only: a modified client could then send any params, and every receiving client
would have to validate bodies itself before rendering.

## 3. Sender kinds

The sender's kind is derived by message-svc, never claimed by the client:

- **agent**: the sender has a row in `messaging.agent_owner`, that is, an owned agent
  enrolled through auth-svc (`entra-app` or `agent-key`) whose ownership the worker relayed
  to message-svc.
- **human**: everyone else.

Legacy linked-only app accounts, which predate owned agents and are not known to messaging
as agents, count as **human**: they may send templates open to humans, not agent-only ones.
A misclassification never lets a person send an agent-only template. Any kind may answer a
card.

## 4. tpl/1 reference

A definition is one JSON object of at most **16 KiB** (16384 bytes) of UTF-8, parsed
strictly: unknown fields are refused at every level, as are duplicate keys anywhere,
nesting deeper than 16 levels and anything after the object. The validator is pure code in
`push-domain::templates`, shared by message-svc and `push-sdk`. Every refusal names the
rule and where it failed, for example `layout[2].param: no param is named "body"`.

### Top-level fields

| Field | Required | Rule |
|---|---|---|
| `format` | yes | exactly `"tpl/1"` |
| `publisher` | yes | id: `^[a-z][a-z0-9-]{0,31}$`; claimed by the first account that submits under it (see [publishing](#7-publishing-templates)) |
| `name` | yes | id: `^[a-z][a-z0-9-]{0,31}$` |
| `version` | yes | integer 1 to 65535 |
| `title` | yes | literal wording, 1 to 80 characters, one line |
| `senders` | yes | 1 or 2 distinct of `"agent"`, `"human"` |
| `params` | yes | object, 0 to 16 params; names `^[a-z][a-z0-9_]{0,31}$` |
| `layout` | yes | array of 1 to 12 blocks |

A version is named by its **ref**, `<publisher>.<name>@<version>`, the version in decimal
without a sign or leading zero: `acme.deploy@1`.

### Param types

Every param may add `"optional": true` (default `false`). Every param must be used by at
least one block; a param may be used by several.

| `type` | Fields | Value in a body |
|---|---|---|
| `string` | `max_len` 1 to 4000 (characters), `multiline` (default `false`) | a string, 1 to `max_len` characters, not only whitespace; line breaks (`\n`) only when `multiline` |
| `enum` | `values`: 1 to 16 distinct ids `^[a-z][a-z0-9_]{0,31}$` | a string, one of `values` |
| `int` | `min`, `max`: integers within ±(2^53 − 1), `min` ≤ `max` | a JSON integer from `min` to `max` |
| `bool` | none | `true` or `false` |
| `list` | `item` (below), `max_items` 1 to 50 | an array of 0 to `max_items` items |

List item types (`item`):

| `item.type` | Fields | Item in a body |
|---|---|---|
| `string` | `max_len` 1 to 500 | a string, 1 to `max_len` characters, one line, not only whitespace |
| `option` | none | `{"id", "label"}`: `id` `^[a-z][a-z0-9_]{0,31}$`, distinct within the list; `label` 1 to 80 characters, one line, not only whitespace |
| `step` | none | `{"label", "status", "detail"?}`: `label` 1 to 200 characters, one line, not only whitespace; `status` one of `pending`, `running`, `done`, `failed`, `skipped`; `detail` at most 1000 characters, line breaks allowed |

### Blocks

| `type` | Fields | Accepts |
|---|---|---|
| `heading` | exactly one of `text` (literal, 1 to 200 characters, one line) or `param` | a single-line `string` param whose `max_len` is at most 200 |
| `text` | exactly one of `text` (literal, 1 to 500 characters, line breaks allowed) or `param` | a `string` param (single- or multi-line), or an `int`, `bool`, `enum` or `list` of `string` param, shown as plain text |
| `preview` | `param`, `label`? | a `string` param (content shown verbatim, e.g. a command or a diff) |
| `thinking` | `param`, `label`? | a `string` param (the agent's reasoning, usually collapsed) |
| `steps` | `param`, `label`? | a `list` of `step` param |
| `status` | `param`, `label`? | an `enum` param |
| `buttons` | `options`: 1 to 4 `{"id", "label", "style"}`; `style` is `primary`, `secondary` or `danger` | literal options only |
| `single` | `label`?, exactly one of `options` (1 to 10 literal `{"id", "label"}`) or `param` | a required `list` of `option` param |
| `multi` | as `single`, plus `min` and `max` | as `single`; 1 ≤ `min` ≤ `max` ≤ the number of literal options, or the param's `max_items` |

Block labels are literal wording, 1 to 80 characters, one line. Option ids match
`^[a-z][a-z0-9_]{0,31}$` and are distinct within a block; option labels are literal
wording, 1 to 80 characters.

`buttons`, `single` and `multi` are **interactive**: they make the message a card that can
be answered. A block whose optional param is absent from a body is skipped when rendering.

### Meta-rules

A definition is refused unless all of these hold:

1. It is at most 16 KiB, valid UTF-8, strict JSON (no duplicate keys, depth ≤ 16), a JSON
   object, with no unknown field at any level.
2. `format` is `tpl/1`; `publisher` and `name` are ids; `version` is 1 to 65535.
3. `senders` has 1 or 2 distinct kinds.
4. At most 16 params and 12 blocks, at least one block; param names and enum values match
   `^[a-z][a-z0-9_]{0,31}$`; enum values are distinct.
5. Every cap is within its range (`max_len`, `max_items`, `min`/`max`, option counts).
6. No unknown param type, item type or block type.
7. Every block that names a `param` names an existing param of a type the block accepts
   (table above); a `heading` param is single-line with `max_len` ≤ 200.
8. Every param is used by at least one block.
9. At most one interactive block, and it does not use an optional param.
10. `multi` bounds: 1 ≤ `min` ≤ `max` ≤ the options it can offer.
11. Option ids are distinct within a block.
12. All literal wording passes the wording rules below.
13. The canonical form (below) also fits in 16 KiB.

### Literal wording rules

Literal wording is everything a definition itself says: `title`, block `label`s, literal
`heading` and `text`, and option `label`s. It is what an approved template shows on every
card, so it is held to stricter rules than the values a sender fills in:

- Length: `title`, labels and option labels 1 to 80 characters; a literal `heading` 1 to
  200; a literal `text` 1 to 500. Never empty or only whitespace.
- No control characters; a literal `text` may hold line breaks (`\n`), nothing else may.
- No line or paragraph separators (U+2028, U+2029) and no invisible or formatting
  characters: soft hyphen, combining grapheme joiner, Arabic letter mark, Hangul fillers,
  zero-width characters (U+200B to U+200F), bidi embeddings and isolates (U+202A to
  U+202E, U+2060 to U+206F), variation selectors, the braille blank, the BOM, interlinear
  annotation controls, shorthand and musical format controls, and tag characters.
- No markup: `<`, `>`, `` ` ``, `{`, `}` or `](`.
- Nothing URL-like. The text is first normalised: NFKC (so fullwidth letters and similar
  compatibility forms count as the characters they stand for), the dot lookalikes U+3002,
  U+FF61, U+FE52 and U+2024 read as `.`, then lower-cased. It is URL-like if it contains:
  - `//` or `www.`;
  - a known scheme at the start of a word: `javascript:`, `data:`, `vbscript:`, `file:`,
    `mailto:`, `tel:`, `sms:`, `blob:`, `about:`, `intent:`, `ftp:`, `http:`, `https:`;
  - a generic scheme: two or more ASCII letters at the start of a word, `:`, then a
    character that is neither whitespace nor a digit (`magnet:?`, `tg:x` and `Note:x` are
    refused; `Ready: yes` and `at 10:30` are fine);
  - a dotted-quad IPv4 address (`10.0.0.1`);
  - a dotted host ending in a common top-level domain (`example.com`, `EXAMPLE。COM`):
    `com`, `net`, `org`, `io`, `dev`, `app`, `ai`, `co`, `info`, `biz`, `me`, `xyz`, `ly`,
    `gl`, `gg`, `to`, `sh`, `ru`, `cn`, `uk`, `de`, `fr`, `eu`, `us`, `ca`, `au`, `in`,
    `jp`, `tv`, `cc`, `link`, `site`, `online`, `top`, `club`, `page`. Labels are compared
    whole, so `v1.2.3` and `file.txt` are fine.

Values in bodies follow lighter rules ([section 5](#5-message-bodies-and-card-state)):
URLs and markup are allowed there, because clients render them as inert text.

### Canonical form

message-svc stores, and the catalog serves, the **canonical form** of a definition:
compact JSON, top-level keys in the order `format`, `publisher`, `name`, `version`,
`title`, `senders`, `params`, `layout`, params and blocks in definition order, and defaults
left out (`"multiline": false` and `"optional": false` are dropped). It parses back to the
same definition. The catalog hands it to clients as a JSON object whose key order is not
significant: compare definitions by their parsed content, never by hashing bytes.

### A complete example

```json
{
  "format": "tpl/1",
  "publisher": "acme",
  "name": "deploy",
  "version": 1,
  "title": "Deployment request",
  "senders": ["agent", "human"],
  "params": {
    "service": {"type": "string", "max_len": 80},
    "stage": {"type": "enum", "values": ["queued", "running", "done", "failed"]},
    "summary": {"type": "string", "max_len": 2000, "multiline": true},
    "replicas": {"type": "int", "min": 1, "max": 50},
    "canary": {"type": "bool", "optional": true},
    "changes": {"type": "list", "item": {"type": "string", "max_len": 200}, "max_items": 20, "optional": true},
    "plan": {"type": "list", "item": {"type": "step"}, "max_items": 10, "optional": true},
    "regions": {"type": "list", "item": {"type": "option"}, "max_items": 8}
  },
  "layout": [
    {"type": "heading", "param": "service"},
    {"type": "status", "param": "stage", "label": "Stage"},
    {"type": "text", "param": "summary"},
    {"type": "text", "param": "replicas"},
    {"type": "text", "param": "canary"},
    {"type": "text", "param": "changes"},
    {"type": "steps", "param": "plan", "label": "Plan"},
    {"type": "text", "text": "Pick the regions to deploy to."},
    {"type": "multi", "label": "Regions", "param": "regions", "min": 1, "max": 3}
  ]
}
```

Its outline, as the review API shows it:

```text
acme.deploy@1 "Deployment request" senders: agent, human
heading: {service}
status "Stage": {stage}
text: {summary}
text: {replicas}
text: {canary} (optional)
text: {changes} (optional)
steps "Plan": {plan} (optional)
text: "Pick the regions to deploy to."
multi "Regions": {regions} (choose 1 to 3)
params:
  service: string, max_len 80
  stage: enum [queued, running, done, failed]
  summary: string, max_len 2000, multiline
  replicas: int, 1 to 50
  canary: bool, optional
  changes: list of string (max_len 200), max_items 20, optional
  plan: list of step, max_items 10, optional
  regions: list of option, max_items 8
```

## 5. Message bodies and card state

RICH and ANSWER messages are sent like text, in a `Send` frame with `content_type`
`CONTENT_TYPE_RICH` (3) or `CONTENT_TYPE_ANSWER` (4) and the JSON body as bytes. Bodies are
strict JSON too (no duplicate keys, depth ≤ 16, nothing after the object, no unknown
field). Any other content type than `TEXT`, `RICH` or `ANSWER` is refused.

### RICH body

At most **32 KiB**: exactly `tpl`, `params` and `fallback`.

| Field | Rule |
|---|---|
| `tpl` | a ref of an approved version |
| `params` | an object: every required param present, no unknown one, each value as its [type](#param-types) says |
| `fallback` | 1 to 2000 characters of plain text, line breaks allowed, not only whitespace: what clients show when they cannot render the card |

Value rules, for every string in a body (param values, list items, labels, step details,
`fallback`): at most its cap in characters; no control characters (`\n` only where the
type allows line breaks); no U+2028 or U+2029; no invisible or formatting characters (the
same set as for literal wording). Strings that are values (`string` params, `string` items,
option and step labels) must not be empty or only whitespace; a step `detail` may be empty.
URLs and markup **are** allowed in values: clients show them as inert text.

When the card's interactive block takes its options from a `list` of `option` param, the
list must offer at least one option for `single`, at least `min` for `multi`.

```json
{
  "tpl": "acme.deploy@1",
  "params": {
    "service": "billing-api",
    "stage": "queued",
    "summary": "Roll out 2.4.1 with the invoice rounding fix.\nDatabase migrations: none.",
    "replicas": 3,
    "changes": ["Fix rounding of invoice totals", "Bump the HTTP client"],
    "plan": [
      {"label": "Build image", "status": "done"},
      {"label": "Run tests", "status": "running", "detail": "412 of 980 passed"}
    ],
    "regions": [
      {"id": "eu_west", "label": "EU West"},
      {"id": "us_east", "label": "US East"}
    ]
  },
  "fallback": "Deploy billing-api 2.4.1 (3 replicas)? Choose the regions: EU West, US East."
}
```

With a built-in:

```json
{
  "tpl": "agent.approval@1",
  "params": {
    "title": "Delete 3 stale branches?",
    "details": "feature/old-login, fix/typo and spike/grpc have had no commits for 90 days.",
    "preview": "git push origin --delete feature/old-login fix/typo spike/grpc"
  },
  "fallback": "Approval request: delete 3 stale branches? Reply in a client that shows cards."
}
```

### ANSWER body

At most **4 KiB**: exactly `card`, `values` and `fallback`.

| Field | Rule |
|---|---|
| `card` | the card's message id, a hyphenated UUID (36 characters) |
| `values` | 1 to 10 distinct option ids (`^[a-z][a-z0-9_]{0,31}$`), each offered by the card: its literal options, or the ids in the card's own `list` of `option` param. Exactly one for `buttons` and `single`; `min` to `max` for `multi` |
| `fallback` | 1 to 500 characters, one line, not only whitespace, e.g. the chosen labels |

```json
{"card": "0b9f5c1e-6a0d-4d0e-9a51-6f1c2d3e4f50", "values": ["eu_west", "us_east"], "fallback": "EU West, US East"}
```

### Card state

Card state is kept by the server, so a sender never edits a card to close it:

- The card must be a RICH message of the same conversation that the answerer may see (not
  before their `visible_from_seq`), not deleted, whose template is still approved and has
  an interactive block. Any sender kind may answer.
- Answering one's own card is refused.
- The **first valid answer closes the card**: in the same transaction the card records the
  answer's seq, delivered as `Deliver.answer_seq` on the card (in history and sync). The
  answer itself fans out like any message; its `Deliver.target_seq` is the card's seq.
- A second answer is refused with `ERROR_CODE_REJECTED` ("the card is already answered");
  the database also refuses to change a recorded answer.
- An answered card cannot be edited (`REJECTED`). Answers can never be edited.
- Deleting an answer does not reopen the card: it keeps its `answer_seq`.
- A retry of a send (same `client_msg_id`) gets its original receipt, even if the template
  was revoked meanwhile.

Edits are checked as the **target's** content type: a TEXT message stays text, a card is
replaced by a full RICH body checked as on send (any approved template the sender's kind
may use, with fresh params), an ANSWER cannot be edited. The usual edit rules (own message,
edit window, not deleted) apply first. Delete is unchanged.

### Error codes

| Refusal | `ErrorCode` |
|---|---|
| Content type unspecified or `SYSTEM_EVENT` | `ERROR_CODE_INVALID` |
| RICH or ANSWER body too large, not strict JSON, unknown or missing field, bad `fallback` | `ERROR_CODE_INVALID` |
| The ref is missing, pending, rejected or revoked (one refusal: "template … is not approved") | `ERROR_CODE_INVALID` |
| A param is unknown, missing, of the wrong type or beyond its caps; a choice list offers too few options | `ERROR_CODE_INVALID` |
| The sender's kind is not in `senders` | `ERROR_CODE_INVALID` |
| The answered message does not exist in the conversation, is deleted or is not visible to the answerer | `ERROR_CODE_TARGET_NOT_FOUND` |
| The answered message is not a card (not RICH) | `ERROR_CODE_INVALID` |
| Answering one's own card | `ERROR_CODE_INVALID` |
| The card's template is no longer approved, or has no interactive block | `ERROR_CODE_INVALID` |
| A value the card does not offer, or the wrong number of values | `ERROR_CODE_INVALID` |
| The card is already answered | `ERROR_CODE_REJECTED` |
| Editing an answered card | `ERROR_CODE_REJECTED` |
| Editing an answer | `ERROR_CODE_INVALID` |
| Editing someone else's message, after the window, or a deleted one | `ERROR_CODE_NOT_SENDER`, `ERROR_CODE_WINDOW_EXPIRED`, `ERROR_CODE_ALREADY_DELETED` (unchanged) |

For a template refusal the error's `message` names the rule that failed and where
(prefixed `invalid template:`), e.g. `invalid template: params.replicas: must be 1 to 50,
not 80`.

### SDK

`push-sdk` maps content types 3 and 4 to `MessageKind::Rich` and `MessageKind::Answer`,
keeps `answer_seq` on cards and applies incoming answers to their card, and offers
`send_rich` and `send_answer` (Node `sendRich`/`sendAnswer`, C `push_client_send_rich`/
`push_client_send_answer`) with the same envelope checks before queueing. See
[docs/sdk-bindings.md](sdk-bindings.md#cards-and-answers). The SDK does not fetch the
catalog yet; apps call `GET /v1/templates` themselves ([section 9](#9-client-catalog-protocol)).

## 6. Built-in templates

Four `agent.*` templates ship in `push-domain` (`crates/push-domain/src/templates/builtin/`),
validated by the same code as any submission. message-svc **seeds them as approved at
start-up**, before it reports ready, so the very first catalog lists them. They are owned
by the server: the publisher `agent` is reserved and claimed by the server, so no account
can publish under it. Seeding inserts missing versions only, so an admin may **revoke** a
built-in and the revocation survives restarts. A changed built-in must get a new version;
message-svc warns (`templates.builtin_mismatch`) when a stored built-in differs from the
shipped one. All four are for agents only.

These were designed upstream: the consumer's own spec
(`docs/specs/2026-10-26-rich-messages.md` in the consumer repository) was not available
when push-messenger built them. The consumer should reconcile its spec with tpl/1 and the
definitions below, and propose any change as **new versions** (`agent.approval@2`, …),
since a stored version never changes.

### `agent.reply@1`

An agent's answer: an optional heading, its thinking and tool steps, then the reply text.

```json
{
  "format": "tpl/1",
  "publisher": "agent",
  "name": "reply",
  "version": 1,
  "title": "Agent reply",
  "senders": ["agent"],
  "params": {
    "title": {"type": "string", "max_len": 120, "optional": true},
    "text": {"type": "string", "max_len": 4000, "multiline": true},
    "thinking": {"type": "string", "max_len": 4000, "multiline": true, "optional": true},
    "steps": {"type": "list", "item": {"type": "step"}, "max_items": 50, "optional": true}
  },
  "layout": [
    {"type": "heading", "param": "title"},
    {"type": "thinking", "param": "thinking", "label": "Thinking"},
    {"type": "steps", "param": "steps", "label": "Steps"},
    {"type": "text", "param": "text"}
  ]
}
```

### `agent.approval@1`

A yes/no decision: what the agent wants to do, an optional preview (a command, a diff),
and Approve (primary) or Reject (danger) buttons.

```json
{
  "format": "tpl/1",
  "publisher": "agent",
  "name": "approval",
  "version": 1,
  "title": "Approval request",
  "senders": ["agent"],
  "params": {
    "title": {"type": "string", "max_len": 120},
    "details": {"type": "string", "max_len": 4000, "multiline": true},
    "preview": {"type": "string", "max_len": 4000, "multiline": true, "optional": true}
  },
  "layout": [
    {"type": "heading", "param": "title"},
    {"type": "text", "param": "details"},
    {"type": "preview", "param": "preview", "label": "Preview"},
    {"type": "buttons", "options": [
      {"id": "approve", "label": "Approve", "style": "primary"},
      {"id": "reject", "label": "Reject", "style": "danger"}
    ]}
  ]
}
```

### `agent.question@1`

A question with up to 10 options supplied by the agent; the person picks one.

```json
{
  "format": "tpl/1",
  "publisher": "agent",
  "name": "question",
  "version": 1,
  "title": "Question",
  "senders": ["agent"],
  "params": {
    "question": {"type": "string", "max_len": 1000, "multiline": true},
    "options": {"type": "list", "item": {"type": "option"}, "max_items": 10}
  },
  "layout": [
    {"type": "text", "param": "question"},
    {"type": "single", "label": "Choose one", "param": "options"}
  ]
}
```

### `agent.plan@1`

A plan or a progress report: a heading, an optional overall state and up to 50 steps. Not
interactive; edit the card (with a full RICH body) as work progresses.

```json
{
  "format": "tpl/1",
  "publisher": "agent",
  "name": "plan",
  "version": 1,
  "title": "Plan",
  "senders": ["agent"],
  "params": {
    "title": {"type": "string", "max_len": 120},
    "steps": {"type": "list", "item": {"type": "step"}, "max_items": 50},
    "state": {"type": "enum", "values": ["draft", "running", "done", "failed"], "optional": true}
  },
  "layout": [
    {"type": "heading", "param": "title"},
    {"type": "status", "param": "state", "label": "State"},
    {"type": "steps", "param": "steps", "label": "Steps"}
  ]
}
```

## 7. Publishing templates

Any signed-in account submits a definition to auth-svc with its access token:

```bash
curl -sS -X POST "$PUSH_AUTH_URL/v1/templates" \
  -H "Authorization: Bearer $ACCESS_TOKEN" \
  -H "Content-Type: application/json" \
  --data-binary @acme.deploy.json
```

```json
{"template": "acme.deploy@1", "status": "pending", "title": "Deployment request", "submitted_at": "2026-10-26T17:30:00.123Z"}
```

The answer is 201. The body (`application/json`, at most 16 KiB) is forwarded to
message-svc byte for byte, which parses it strictly and stores its canonical form.

- **Publisher namespaces.** The first accepted submission under a publisher claims it for
  the submitting account, for good. Later versions and other names under that publisher
  must come from the same account (otherwise 409 `conflict`, "the publisher belongs to
  another account").
- **Reserved publishers:** `agent`, `push`, `system`, `admin` and `builtin` (409
  `conflict`, "the publisher is reserved"). The server owns `agent`.
- **Pending cap:** at most 20 versions per account waiting for review (409 `conflict`).
- **Rate limits:** 10 submissions per hour per account and 60 per hour per IP (429
  `rate_limited` with `Retry-After`).
- **Immutability:** a stored version never changes (a database trigger refuses it), and
  versions are never deleted. A change is a new version. A ref that is stored in any
  status, including rejected and revoked, cannot be submitted again (409 `conflict`, "this
  template version already exists"): bump the version.

| Status | Code | When |
|---|---|---|
| 201 | | stored as `pending` |
| 400 | `invalid` | not JSON or over 16 KiB, or a [meta-rule](#meta-rules) failed (the message names it) |
| 401 | `unauthorized`, `token_expired` | no or a bad access token |
| 409 | `conflict` | the ref exists, the publisher is reserved or another account's, too many pending |
| 429 | `rate_limited` | see above |
| 503 | `unavailable` | message-svc did not answer, or does not know the account yet (new accounts reach it asynchronously, in about a second): retry |

Errors are `{"error": {"code", "message", "retryable"}}`.

Status lifecycle (one way; `rejected` and `revoked` are final):

```text
 submit (an account)          approve                   revoke
 ───────────────────► pending ────────► approved ────────► revoked   (final)
                         │                  ▲
                         │ reject           │ seed (the server, built-ins at start-up)
                         ▼
                      rejected   (final)
```

Only `approved` and `revoked` versions appear in the client catalog.

## 8. Review (admins)

The template **admins are the holders of `PUSH_AUTH_ADMIN_TOKEN`**. The review routes are
served only on auth-svc's admin listener (`PUSH_AUTH_ADMIN_ADDR`, `:8082`), which exists
only when the token is set and which the chart never routes through the Ingress (reach it
with a port-forward from an allowed peer, `networkPolicy.adminPeers`). There are no
per-user reviewer accounts: each review call names its `reviewer`, a label of 1 to 64
printable characters kept in the audit trail. Calls are limited to 60 per 15 minutes per
IP; failed token attempts to 20 per 15 minutes per IP. Like the public routes, they need
`PUSH_MESSAGE_SVC_URL` and `PUSH_DIRECTORY_TOKEN` (otherwise 503).

```bash
kubectl port-forward -n push deploy/push-auth 8082:8082
ADMIN="http://127.0.0.1:8082"
AUTHZ="Authorization: Bearer $PUSH_AUTH_ADMIN_TOKEN"
```

List versions, newest submission first (at most 200), optionally by status
(`pending`, `approved`, `rejected`, `revoked`):

```bash
curl -sS -H "$AUTHZ" "$ADMIN/v1/admin/templates?status=pending"
```

```json
{"templates": [{"template": "acme.deploy@1", "status": "pending", "title": "Deployment request",
  "submitted_by": "6f1c2d3e-…", "submitted_at": "2026-10-26T17:30:00.123Z", "reviewed_at": null, "seq": null}]}
```

Show one version: the summary's fields, the canonical `definition`, a plain-text
`outline` (the preview: one line per block, then the params; see the
[example](#a-complete-example)) and `events`, its audit trail, oldest first:

```bash
curl -sS -H "$AUTHZ" "$ADMIN/v1/admin/templates/acme.deploy@1"
```

```json
{"template": "acme.deploy@1", "status": "pending", "title": "Deployment request",
 "submitted_by": "6f1c2d3e-…", "submitted_at": "…", "reviewed_at": null, "seq": null,
 "definition": {"format": "tpl/1", "…": "…"},
 "outline": "acme.deploy@1 \"Deployment request\" senders: agent, human\nheading: {service}\n…",
 "events": [{"action": "submit", "actor": "6f1c2d3e-…", "reviewer": null, "note": null, "at": "…"}]}
```

Approve or reject a pending version, revoke an approved one. The body is
`{"reviewer", "note"?}` and nothing else; `note` is at most 500 characters, line breaks
allowed:

```bash
curl -sS -X POST -H "$AUTHZ" -H "Content-Type: application/json" \
  -d '{"reviewer": "erin", "note": "Checklist passed"}' \
  "$ADMIN/v1/admin/templates/acme.deploy@1/approve"
curl -sS -X POST -H "$AUTHZ" -H "Content-Type: application/json" \
  -d '{"reviewer": "erin", "note": "Imitates a sign-in prompt"}' \
  "$ADMIN/v1/admin/templates/acme.login@1/reject"
curl -sS -X POST -H "$AUTHZ" -H "Content-Type: application/json" \
  -d '{"reviewer": "erin", "note": "Superseded by acme.deploy@2"}' \
  "$ADMIN/v1/admin/templates/acme.deploy@1/revoke"
```

Each answers the version's summary. Approving and revoking take the next catalog change
number (`seq`), so clients see the change on their next fetch; a revocation is enforced by
message-svc on the very next send. 400 `invalid` (a bad ref, reviewer or note), 404
`not_found` (no such version), 409 `conflict` (approve or reject of a version that is not
pending, revoke of one that is not approved).

### Review checklist

Before approving, check the outline and the definition:

- [ ] **Wording is honest** and does not imitate system or security UI: no "Your session
      expired", "Verify your account", "Security alert", fake system notices or warnings.
- [ ] **No brand impersonation**: the title, labels and literal text do not pose as
      another product, company or person, and the publisher id is not a look-alike of one.
- [ ] **Senders are appropriate**: a template styled as an agent's (approvals, plans,
      tool steps, "thinking") is `["agent"]` only; `human` only for content people
      legitimately send.
- [ ] **Params are minimal** with tight caps: no 4000-character string where 120 would do,
      no free-text param that could carry a whole fake UI, `max_items` as small as useful.
- [ ] **Interactive options are clear and safe**: every option says what it does,
      destructive or irreversible actions are styled `danger`, and nothing invites a click
      that does more than its label says.
- [ ] **`fallback` will be adequate**: the template's purpose is understandable as plain
      text for clients that cannot render it.
- [ ] **No collection of personal data**: no prompts for passwords, codes, card numbers,
      addresses or other PII, whether in literal wording or by design of the params.
- [ ] **The outline was reviewed**, block by block, including which params are optional
      (a card may look quite different with them absent).
- [ ] **The publisher's identity was checked**: `submitted_by` is an account you expect to
      publish under this publisher.
- [ ] **Older versions**: when approving a new version that fixes a problem, consider
      revoking the older ones.

## 9. Client catalog protocol

Clients fetch the approved templates from auth-svc with their access token:

```bash
curl -sS -H "Authorization: Bearer $ACCESS_TOKEN" "$PUSH_AUTH_URL/v1/templates?since=0"
```

```json
{
  "seq": 6,
  "more": false,
  "templates": [
    {"template": "agent.reply@1", "status": "approved", "seq": 1, "definition": {"format": "tpl/1", "…": "…"}},
    {"template": "agent.question@1", "status": "approved", "seq": 3, "definition": {"…": "…"}},
    {"template": "agent.plan@1", "status": "approved", "seq": 4, "definition": {"…": "…"}},
    {"template": "agent.approval@1", "status": "revoked", "seq": 5},
    {"template": "acme.deploy@1", "status": "approved", "seq": 6, "definition": {"…": "…"}}
  ]
}
```

- `since` (default 0, a whole number ≥ 0) is the position to read from; the answer lists
  the versions approved, revoked or seeded **after** it, oldest change first, at most 200
  per page. Each version appears at its latest change only. `definition` (the canonical
  definition as a JSON object) is present for `approved` entries only.
- `seq` is the position to ask from next; `more: true` means more changes already wait
  beyond this page. Change numbers commit in order, so a client that has read up to `N`
  never misses a change below `N`.
- Loop: start from the stored `seq` (0 the first time), fetch, apply the entries, store
  the new `seq`, and fetch again at once while `more` is `true`.
- The answer carries `ETag: "tpl-<since>-<seq>"` and `Cache-Control: private, no-cache`.
  Send `If-None-Match` with the ETag of the last answer for the same `since` (or `*`) and
  an unchanged catalog answers **304** with no body. After a full read, nothing new means
  `seq` = `since`, so a client polling from `S` can always send `If-None-Match: "tpl-S-S"`.
- If an answer's `seq` is lower than the `since` you sent, the server's registry was
  restored to an earlier state: drop the stored catalog and read again from 0.
- Rate limits: 120 per minute per account, 600 per minute per IP (429 `rate_limited`
  with `Retry-After`). 400 `invalid` for a bad `since`, 503 `unavailable` when message-svc
  does not answer.

Storing the catalog: keep a table of ref → status and definition, and the `seq`. On an
`approved` entry store or replace the definition; on a `revoked` entry mark the ref
revoked (and drop its definition). A revoked version must **no longer be rendered**, in
new and in existing messages: show those messages' `fallback`. A message whose ref is not
in the catalog (yet) also shows its `fallback`; fetch the catalog and render it once the
ref arrives. Definitions are JSON objects whose key order is not significant: never hash
or compare their bytes.

Polling: fetch at start-up (after sign-in), on every reconnect of the WebSocket, when a
message names a ref you do not know, and every few minutes (e.g. every 5) while the app is
in the foreground. With `If-None-Match` almost every poll is a 304.

## 10. Audit

Every submission, approval, rejection, revocation and seeding is a row in
`messaging.template_event`, append-only (triggers refuse UPDATE, DELETE and TRUNCATE):

| Column | Meaning |
|---|---|
| `publisher`, `name`, `version` | the version |
| `action` | `submit`, `approve`, `reject`, `revoke` or `seed` |
| `actor` | the submitting account (`submit`); NULL for reviews and seeding |
| `reviewer` | the reviewer label given to the admin API (`approve`, `reject`, `revoke`) |
| `note` | the review note, if any |
| `at` | when |

The admin API shows a version's events (`GET /v1/admin/templates/{ref}`).

Security log events (target `security`; no definition or message body is ever logged):

| Service | Event | When |
|---|---|---|
| message-svc | `templates.submitted` | a version was stored as pending (ref, user) |
| message-svc | `templates.approved`, `templates.rejected`, `templates.revoked` | a review (ref, reviewer) |
| message-svc | `templates.seeded` | built-ins were stored at start-up (count) |
| message-svc | `templates.builtin_mismatch` | a stored built-in differs from the shipped one (warning) |
| auth-svc | `auth.template.submitted` | `POST /v1/templates` answered 201 |
| auth-svc | `auth.template.refused` | `POST /v1/templates` answered 409 |
| auth-svc | `auth.admin.template_reviewed` | a review on the admin listener (ref, action, status, reviewer, **IP**) |
| auth-svc | `auth.admin.token.refused` | an admin call without the right token (IP) |
| auth-svc | `auth.rate_limited`, `auth.token.refused` | 429 and 401 answers, as for every route |

auth-svc's request events record the caller's IP, user and device; the reviewer's IP is
recorded only in auth-svc's log, not in the database.

## 11. Limits and follow-ups

| Limit | Value |
|---|---|
| Definition (submitted and canonical) | 16 KiB |
| RICH body / ANSWER body | 32 KiB / 4 KiB |
| Params / blocks per definition | 16 / 12 |
| Interactive blocks per definition | 1 |
| `string` param `max_len` | 1 to 4000 characters |
| `enum` values | 1 to 16 |
| `int` bounds | ±(2^53 − 1) |
| `list` `max_items` / `string` item `max_len` | 1 to 50 / 1 to 500 |
| Option label / step label / step detail | 80 / 200 / 1000 characters |
| `buttons` options / `single` and `multi` literal options | 1 to 4 / 1 to 10 |
| Title, labels / literal heading / literal text | 80 / 200 / 500 characters |
| Heading-bound param `max_len` | ≤ 200 |
| RICH `fallback` / ANSWER `fallback` | 2000 / 500 characters |
| ANSWER values | 1 to 10 |
| Strict JSON nesting depth | 16 |
| Pending submissions per account | 20 |
| Submissions | 10 per hour per account, 60 per hour per IP |
| Catalog fetches | 120 per minute per account, 600 per minute per IP |
| Catalog page / admin listing | 200 entries |
| Reviewer label / review note | 1 to 64 / ≤ 500 characters |
| Admin listener | 60 per 15 minutes per IP; 20 failed attempts per 15 minutes per IP |

No new configuration: the public and admin template routes use auth-svc's existing
`PUSH_MESSAGE_SVC_URL` and `PUSH_DIRECTORY_TOKEN` to reach message-svc's internal
`push.internal.v1.Templates` service (on the Directory port), and the admin routes the
existing `PUSH_AUTH_ADMIN_TOKEN`. The schema is messaging migration
`0005_templates.sql`.

Follow-ups, not in this release:

- **Catalog fetch in the SDK**: apps call `GET /v1/templates` themselves for now.
- **Per-user reviewer accounts** on the public API (rejected for now: it would put an
  approval endpoint on the internet-facing listener).
- **Localisation** of literal wording.
- **End-to-end encryption**, which would move enforcement to a cleartext envelope
  ([section 2](#2-enforcement-model)).
