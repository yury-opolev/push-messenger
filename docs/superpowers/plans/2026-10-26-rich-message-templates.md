# Rich message templates — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps
> use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reviewed, server-enforced rich message templates (tpl/1), a template registry with
admin review and a client catalog, `CONTENT_TYPE_RICH` / `CONTENT_TYPE_ANSWER`, server card
state, and four built-in `agent.*` templates (REQUEST PUSH feature 8).

**Architecture:** Spec: `docs/superpowers/specs/2026-10-26-rich-message-templates-design.md`
(decisions D1-D13). Pure tpl/1 code in `push-domain::templates`; registry and enforcement
as `Messaging` operations over new `Tx` methods (in-memory and Postgres stores, one
behaviour suite in push-testkit); message-svc serves a new internal gRPC `Templates`
service; auth-svc serves the HTTP API (public catalog/submit, admin review).

**Tech stack:** Rust 2024 (MSRV 1.85: no let-chains), serde/serde_json, sqlx/Postgres 17,
tonic/prost, axum, testcontainers (Docker).

**Gates (every task):** `cargo fmt --all -- --check`;
`cargo clippy --workspace --all-targets --locked -j 4 -- -D warnings`; the touched crates'
tests with `--locked -j 4`. After adding a migration: `cargo clean -p push-store` before its
tests (no build.rs rerun-if-changed). Never edit existing migrations. Never `git add -A`:
`.github/copilot/` must never be committed; add paths explicitly. Commits are conventional,
ending with `Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>`.

---

## File map

| File | Responsibility |
|---|---|
| `crates/push-domain/src/templates/mod.rs` | Public API of tpl/1: re-exports, `TemplateRef`, `SenderKind`, constants |
| `crates/push-domain/src/templates/strict.rs` | Strict JSON value: duplicate keys refused, depth cap |
| `crates/push-domain/src/templates/wording.rs` | Literal and value text rules (URL-like, markup, control, invisible) |
| `crates/push-domain/src/templates/definition.rs` | tpl/1 definition types, parse, meta-rules, canonical JSON |
| `crates/push-domain/src/templates/body.rs` | RICH and ANSWER bodies: parse and check against a definition |
| `crates/push-domain/src/templates/outline.rs` | Plain-text outline of a definition (admin preview) |
| `crates/push-domain/src/templates/builtin.rs` + `builtin/agent.*.json` | The four built-ins |
| `crates/push-domain/src/registry.rs` | `Messaging` registry operations (submit, review, catalog, list, detail, seed) |
| `crates/push-domain/src/rich.rs` | RICH/ANSWER checks inside sends and edits, card state |
| `crates/push-domain/src/{model,store,memory,send,edit,error,lib}.rs` | New variants, fields, `Tx` methods |
| `crates/push-store/migrations/0005_templates.sql` | Registry tables, triggers, content types 3/4, `answer_seq` |
| `crates/push-store/src/{store,rows,codes}.rs` | Postgres implementation |
| `crates/push-testkit/src/contract/templates.rs`, `scenarios/{templates,rich}.rs` | Shared behaviour suite |
| `crates/push-proto/proto/push/v1/frame.proto`, `.../internal/v1/messaging.proto` | Wire |
| `crates/push-message-svc/src/{grpc,convert,server,templates}.rs` | Send/Edit, `Templates` service, seeding |
| `crates/push-auth-core/src/templates.rs` (+ admin.rs, api.rs, limits.rs, server.rs) | HTTP |
| `crates/push-smoke/tests/templates.rs` | Whole-stack acceptance |
| `crates/push-sdk/...`, `bindings/{ffi,c,node}/...` | Kinds, `answer_seq`, `send_rich`, `send_answer` |
| `docs/rich-messages.md`, `CHANGELOG.md` | Format, review checklist, fetch protocol |

---

### Task 1: tpl/1 definitions (push-domain, pure)

**Files:** create `crates/push-domain/src/templates/{mod,strict,wording,definition}.rs`;
modify `crates/push-domain/src/lib.rs` (`pub mod templates;`).

Interfaces (exact names used by later tasks):

```rust
// mod.rs
pub const FORMAT: &str = "tpl/1";
pub const MAX_DEFINITION_BYTES: usize = 16 * 1024;
pub const MAX_RICH_BYTES: usize = 32 * 1024;
pub const MAX_ANSWER_BYTES: usize = 4 * 1024;
pub const RESERVED_PUBLISHERS: &[&str] = &["agent", "push", "system", "admin", "builtin"];
pub const BUILTIN_PUBLISHER: &str = "agent";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SenderKind { Human, Agent }

/// `<publisher>.<name>@<version>`; Display/FromStr round-trip; ids `^[a-z][a-z0-9-]{0,31}$`,
/// version 1..=65535 with no leading zero or sign.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TemplateRef { pub publisher: String, pub name: String, pub version: u16 }

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TemplateError(pub String); // human-readable: which rule failed and where

// definition.rs
pub struct Definition { /* private fields */ }
impl Definition {
    pub fn parse(json: &[u8]) -> Result<Definition, TemplateError>;
    pub fn template_ref(&self) -> TemplateRef;
    pub fn title(&self) -> &str;
    pub fn senders(&self) -> &[SenderKind];
    pub fn allows(&self, kind: SenderKind) -> bool;
    pub fn params(&self) -> &[(String, ParamSpec)];   // in definition order
    pub fn layout(&self) -> &[Block];
    pub fn interactive(&self) -> Option<&Block>;     // the single buttons/single/multi block
    pub fn canonical_json(&self) -> String;          // stable re-serialisation, round-trips via parse
}
pub enum ParamType { String { max_len: u32, multiline: bool }, Enum { values: Vec<String> },
    Int { min: i64, max: i64 }, Bool, List { item: ItemType, max_items: u32 } }
pub enum ItemType { String { max_len: u32 }, Option, Step }
pub struct ParamSpec { pub ty: ParamType, pub optional: bool }
pub enum Block {
    Heading(Source), Text(Source),
    Preview { param: String, label: Option<String> },
    Thinking { param: String, label: Option<String> },
    Steps { param: String, label: Option<String> },
    Buttons { options: Vec<ButtonOption> },
    Single { label: Option<String>, choices: Choices },
    Multi { label: Option<String>, choices: Choices, min: u32, max: u32 },
    Status { param: String, label: Option<String> },
}
pub enum Source { Literal(String), Param(String) }
pub enum Choices { Literal(Vec<ChoiceOption>), Param(String) }
pub struct ButtonOption { pub id: String, pub label: String, pub style: ButtonStyle }
pub enum ButtonStyle { Primary, Secondary, Danger }
pub struct ChoiceOption { pub id: String, pub label: String }
```

JSON shape (snake_case keys; `type` tags):
definition `{format, publisher, name, version, title, senders, params, layout}`; param spec
`{"type":"string","max_len":N,"multiline":bool?,"optional":bool?}`,
`{"type":"enum","values":[..]}`, `{"type":"int","min":i,"max":i}`, `{"type":"bool"}`,
`{"type":"list","item":{"type":"string","max_len":N}|{"type":"option"}|{"type":"step"},"max_items":N}`;
blocks `{"type":"heading","text":"..."}|{"type":"heading","param":"p"}`, `text` same,
`{"type":"preview"|"thinking"|"steps"|"status","param":"p","label":"..."?}`,
`{"type":"buttons","options":[{"id","label","style"}]}`,
`{"type":"single","label"?, "options":[{"id","label"}]}` or `..."param":"p"`,
`{"type":"multi", ..., "min":N, "max":N}`.

Meta-rules (each a distinct `TemplateError` message naming the path):

1. Size ≤ `MAX_DEFINITION_BYTES`, UTF-8, parsed by `strict.rs` (duplicate keys anywhere
   refused, depth ≤ 16), then typed with `deny_unknown_fields` everywhere. `format == "tpl/1"`.
2. `publisher`, `name`: `^[a-z][a-z0-9-]{0,31}$`; `version` 1..=65535.
3. `title`: literal, 1..=80 chars. `senders`: 1..=2 distinct of `human`/`agent`.
4. `params`: ≤ 16, keys `^[a-z][a-z0-9_]{0,31}$`; string `max_len` 1..=4000; enum 1..=16
   distinct values matching the id pattern; int `min ≤ max`, both within ±(2^53−1); list
   `max_items` 1..=50, item string `max_len` 1..=500.
5. `layout`: 1..=12 blocks; unknown `type` refused; every referenced param exists and has
   the type the block needs (heading/text: string, heading single-line; preview/thinking:
   string; steps: list of step; single/multi param: list of option; status: enum); every
   param is referenced at least once; at most one interactive block (buttons/single/multi);
   buttons 1..=4 options; single/multi literal options 1..=10; option ids unique per block
   and matching the id pattern; multi `1 ≤ min ≤ max ≤ option cap` (literal: count; param:
   the list's `max_items`); an interactive block's param, if any, is not optional.
6. Literal wording via `wording::check_literal(text, max_chars, allow_newlines)`:
   heading literal ≤ 200 (single-line), text literal ≤ 500 (newlines allowed), labels
   (block labels, option labels) ≤ 80 (single-line), title ≤ 80. Not empty or only
   whitespace. Refused: any `char::is_control` except `\n` where allowed; invisible/format
   characters (U+00AD, U+034F, U+061C, U+115F-1160, U+17B4-17B5, U+180E, U+200B-200F,
   U+202A-202E, U+2060-206F, U+3164, U+FE00-FE0F, U+FEFF, U+FFA0, U+E0000-E007F); markup
   characters `<`, `>`, `` ` ``, `{`, `}`, and the sequence `](`; URL-like text
   (`wording::looks_like_url`), case-insensitive: `://`, `//`, `www.`, a scheme among
   `javascript: data: vbscript: file: mailto: tel: sms: blob: about: intent: ftp: http: https:`,
   or a dotted host token (`[a-z0-9-]+(\.[a-z0-9-]+)+`) whose last label is a common TLD
   (`com net org io dev app ai co info biz me xyz ly gl gg to sh ru cn uk de fr eu us ca
   au in jp tv cc link site online top club page`) — `e.g.` and `v1.2` pass.

`wording.rs` also exports `check_value(text, max_chars, multiline)` for param values:
control characters (except `\n` when multiline) and invisible/format characters refused;
URLs and markup are allowed in values (clients render them inert).

- [ ] **Step 1: Write failing unit tests** in each file's `#[cfg(test)] mod tests`:
  `TemplateRef` parse/display round-trip and refusals (`A.b@1`, `a.b@0`, `a.b@65536`,
  `a.b@01`, `a@1`, `a.b.c@1`); `looks_like_url` true for `https://x`, `HTTP://X`, `www.x`,
  `javascript:alert`, `data:text/html`, `example.com`, `a.b.io`, `//host`; false for
  `e.g. this`, `version 1.2`, `Approve`, `Ready: yes`; a minimal valid definition parses;
  one test per meta-rule with exactly-at-cap passing and cap + 1 failing (params 16/17,
  blocks 12/13, buttons 4/5, string max_len 4000/4001, enum 16/17, list max_items 50/51,
  label 80/81, definition 16 KiB/+1); unknown field at top level, in a param and in a block;
  unknown block type; duplicate key; URL in title, in a heading literal, in a button label;
  `<b>` in text; bidi override U+202E in a label; unused param; two interactive blocks;
  heading bound to a multiline string; canonical JSON re-parses to an equal definition.
- [ ] **Step 2:** `cargo test -p push-domain --locked -j 4 templates` → fails (missing).
- [ ] **Step 3: Implement** to pass.
- [ ] **Step 4:** tests pass; fmt; clippy.
- [ ] **Step 5: Commit** `feat(domain): parse and check tpl/1 template definitions`.

### Task 2: RICH and ANSWER bodies, outline, built-ins (push-domain, pure)

**Files:** create `crates/push-domain/src/templates/{body,outline,builtin}.rs`,
`crates/push-domain/src/templates/builtin/agent.{reply,approval,question,plan}.json`.

```rust
// body.rs
pub struct RichBody { pub template: TemplateRef, pub params: Params, pub fallback: String }
pub struct Params(/* name -> ParamValue, private */);
pub enum ParamValue { String(String), Int(i64), Bool(bool), List(Vec<ListItem>) }
pub enum ListItem { String(String), Option { id: String, label: String },
    Step { label: String, status: StepStatus, detail: Option<String> } }
pub enum StepStatus { Pending, Running, Done, Failed, Skipped }
pub struct AnswerBody { pub card: uuid::Uuid, pub values: Vec<String>, pub fallback: String }

impl RichBody {
    /// Envelope only: size ≤ MAX_RICH_BYTES, strict JSON `{tpl, params, fallback}`, a valid
    /// ref, `params` an object, `fallback` 1..=2000 chars via check_value(multiline).
    pub fn parse(body: &[u8]) -> Result<RichBody, TemplateError>;
    /// Params against the schema: every required param present, no unknown param, each
    /// value of its type and within its caps (string chars ≤ max_len, no `\n` unless
    /// multiline, check_value; enum value one of values; int within range; list ≤
    /// max_items, items of the item type; option `{id,label}` id pattern, label ≤ 80
    /// single-line, ids unique; step `{label ≤ 200, status, detail? ≤ 1000 multiline}`).
    pub fn check(&self, definition: &Definition) -> Result<(), TemplateError>;
    /// The option ids the card's interactive block offers (literal or from its param).
    pub fn offered(&self, definition: &Definition) -> Vec<String>;
}
impl AnswerBody {
    /// size ≤ MAX_ANSWER_BYTES; strict `{card, values, fallback}`; card a UUID string;
    /// values 1..=10 distinct ids; fallback 1..=500 chars.
    pub fn parse(body: &[u8]) -> Result<AnswerBody, TemplateError>;
    /// Values against the card: each offered; exactly one for buttons/single, min..=max
    /// for multi. Errors if the definition has no interactive block.
    pub fn check(&self, card: &RichBody, definition: &Definition) -> Result<(), TemplateError>;
}
// outline.rs
pub fn outline(definition: &Definition) -> String; // one line per block, e.g.
// "heading: {title}", "text: \"Literal\"", "buttons: [approve \"Approve\" primary] ..."
// builtin.rs
pub fn builtins() -> Vec<Definition>; // the four, parsed from include_str!; panics never
```

Built-in definitions (all `senders: ["agent"]`, version 1, publisher `agent`):

- `agent.reply@1` "Agent reply": params `title` string 120 optional, `text` string 4000
  multiline, `thinking` string 4000 multiline optional, `steps` list step 50 optional;
  layout heading(title), thinking(thinking, label "Thinking"), steps(steps, label "Steps"),
  text(text).
- `agent.approval@1` "Approval request": `title` string 120, `details` string 4000
  multiline, `preview` string 4000 multiline optional; heading(title), text(details),
  preview(preview, label "Preview"), buttons [approve "Approve" primary, reject "Reject"
  danger].
- `agent.question@1` "Question": params `question` string 1000 multiline, `options`
  list option 10; layout text(question), single(param options, label "Choose one").
- `agent.plan@1` "Plan": `title` string 120, `steps` list step 50, `state` enum
  [draft, running, done, failed] optional; heading(title), status(state, label "State"),
  steps(steps, label "Steps").

- [ ] **Step 1: Failing tests:** each built-in parses and round-trips; a valid approval
  body passes; refusals: missing required param, unknown param, wrong type, string over
  max_len, `\n` in single-line string, U+202E in a value, enum value not listed, int out
  of range, list over max_items, option id duplicate, fallback empty / 2001 chars, body
  over 32 KiB, duplicate key in params; answer: valid `approve`, `maybe` refused, two values
  for buttons refused, question answer from the card's own options, multi min/max, card
  not a UUID; `outline` of approval lists four lines.
- [ ] **Step 2–4:** red, implement, green; fmt; clippy.
- [ ] **Step 5: Commit** `feat(domain): check rich and answer bodies; ship the agent templates`.

### Task 3: Wire (push-proto) and exhaustive matches

**Files:** `crates/push-proto/proto/push/v1/frame.proto`,
`crates/push-proto/proto/push/internal/v1/messaging.proto`; fix compile in
`crates/push-message-svc/src/grpc.rs`, `crates/push-sdk/src/store.rs` only as needed to keep
the workspace building (behaviour comes in later tasks).

frame.proto:
```proto
enum ContentType {
  CONTENT_TYPE_UNSPECIFIED = 0;
  CONTENT_TYPE_TEXT = 1;
  CONTENT_TYPE_SYSTEM_EVENT = 2;
  // A card from an approved template (docs/rich-messages.md): JSON {tpl, params, fallback}.
  CONTENT_TYPE_RICH = 3;
  // An answer to a card in the same conversation: JSON {card, values, fallback}.
  CONTENT_TYPE_ANSWER = 4;
}
// Deliver: target_seq comment adds "and ANSWER messages: the seq of the card"; new field
  // Cards (RICH messages with a button or choice block): the seq of the first valid
  // answer, once answered. Later answers are refused.
  optional int64 answer_seq = 19;
```
messaging.proto (`Templates`, guarded by auth-svc's Directory token):
```proto
// Template registry (REQUEST PUSH feature 8), for auth-svc. Guarded by auth-svc's token.
service Templates {
  rpc Submit(SubmitTemplateRequest) returns (TemplateSummary);
  rpc Catalog(CatalogRequest) returns (CatalogResponse);
  rpc List(ListTemplatesRequest) returns (TemplateList);
  rpc Get(GetTemplateRequest) returns (TemplateDetail);
  rpc Review(ReviewTemplateRequest) returns (TemplateSummary);
}
enum TemplateStatus { TEMPLATE_STATUS_UNSPECIFIED = 0; TEMPLATE_STATUS_PENDING = 1;
  TEMPLATE_STATUS_APPROVED = 2; TEMPLATE_STATUS_REJECTED = 3; TEMPLATE_STATUS_REVOKED = 4; }
enum ReviewAction { REVIEW_ACTION_UNSPECIFIED = 0; REVIEW_ACTION_APPROVE = 1;
  REVIEW_ACTION_REJECT = 2; REVIEW_ACTION_REVOKE = 3; }
message SubmitTemplateRequest { bytes user_id = 1; bytes definition = 2; }
message TemplateSummary { string template_ref = 1; TemplateStatus status = 2;
  optional bytes submitted_by = 3; int64 submitted_at = 4; optional int64 reviewed_at = 5;
  optional int64 change_seq = 6; string title = 7; }
message CatalogRequest { int64 since = 1; uint32 limit = 2; }
message CatalogEntry { string template_ref = 1; TemplateStatus status = 2;
  int64 change_seq = 3; optional string definition = 4; }  // definition: approved only
message CatalogResponse { int64 seq = 1; bool more = 2; repeated CatalogEntry entries = 3; }
message ListTemplatesRequest { TemplateStatus status = 1; uint32 limit = 2; }  // UNSPECIFIED: all
message TemplateList { repeated TemplateSummary templates = 1; }
message GetTemplateRequest { string template_ref = 1; }
message TemplateEvent { string action = 1; optional bytes actor = 2; optional string reviewer = 3;
  optional string note = 4; int64 at = 5; }
message TemplateDetail { TemplateSummary summary = 1; string definition = 2; string outline = 3;
  repeated TemplateEvent events = 4; }
message ReviewTemplateRequest { string template_ref = 1; ReviewAction action = 2;
  string reviewer = 3; optional string note = 4; }
```

- [ ] Steps: edit protos; `cargo build --workspace --locked -j 4`; fix non-exhaustive
  matches minimally (message-svc still refuses non-TEXT sends; SDK maps 3/4 to its existing
  default until Task 11); fmt; clippy; `cargo test -p push-proto --locked`; commit
  `feat(proto): rich and answer content types, card state and the Templates API`.

### Task 4: Domain model, `Tx` methods, in-memory store, contract tests

**Files:** `crates/push-domain/src/{model,store,memory,error,lib}.rs`; every `Entry { .. }`
literal in the workspace gains `answer_seq: None`; `crates/push-testkit/src/contract/
{mod,templates}.rs`.

```rust
// model.rs
pub enum ContentType { Text, SystemEvent, Rich, Answer }
pub struct Entry { /* existing */ pub answer_seq: Option<i64> }  // cards: first answer's seq
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TemplateStatus { Pending, Approved, Rejected, Revoked }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTemplate {
    pub template: TemplateRef, pub title: String, pub definition: String, // canonical JSON
    pub status: TemplateStatus, pub submitted_by: Option<UserId>,       // None: built-in
    pub submitted_at: DateTime<Utc>, pub reviewed_at: Option<DateTime<Utc>>,
    pub change_seq: Option<i64>,   // set on approve/revoke/seed
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateAction { Submit, Approve, Reject, Revoke, Seed }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateEvent { pub template: TemplateRef, pub action: TemplateAction,
    pub actor: Option<UserId>, pub reviewer: Option<String>, pub note: Option<String>,
    pub at: DateTime<Utc> }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublisherOwner { Server, User(UserId) }
// error.rs: new variants (and convert.rs mapping in Task 8)
#[error("invalid template: {0}")] Template(#[from] TemplateError), // INVALID
#[error("the card is already answered")] CardAnswered,             // REJECTED
#[error("template not found")] TemplateNotFound,
#[error("this template version already exists")] TemplateExists,   // ALREADY_EXISTS
#[error("{0}")] TemplateConflict(&'static str),                     // wrong status, owner, ...
// store.rs, Tx:
async fn is_agent(&mut self, user: UserId) -> DomainResult<bool>;  // agent_owner row exists
async fn template(&mut self, template: &TemplateRef) -> DomainResult<Option<StoredTemplate>>;
/// Locks the row until the transaction ends (FOR UPDATE).
async fn lock_template(&mut self, template: &TemplateRef) -> DomainResult<Option<StoredTemplate>>;
async fn insert_template(&mut self, template: StoredTemplate) -> DomainResult<InsertOutcome>;
async fn set_template_status(&mut self, template: &TemplateRef, status: TemplateStatus,
    at: DateTime<Utc>, change_seq: Option<i64>) -> DomainResult<()>;
/// Takes the next change number, locking the single counter row until the transaction
/// ends, so change numbers commit in order.
async fn next_template_change(&mut self) -> DomainResult<i64>;
async fn last_template_change(&mut self) -> DomainResult<i64>;   // 0 when none
/// Approved and revoked versions with change_seq > since, by change_seq, at most limit.
async fn templates_changed_since(&mut self, since: i64, limit: u32) -> DomainResult<Vec<StoredTemplate>>;
async fn templates_with_status(&mut self, status: Option<TemplateStatus>, limit: u32) -> DomainResult<Vec<StoredTemplate>>;
async fn pending_templates_of(&mut self, user: UserId) -> DomainResult<u32>;
/// Locks the publisher row (FOR UPDATE), inserting it for `owner` if missing; returns the
/// owner now recorded.
async fn claim_publisher(&mut self, publisher: &str, owner: PublisherOwner) -> DomainResult<PublisherOwner>;
async fn insert_template_event(&mut self, event: TemplateEvent) -> DomainResult<()>;
async fn template_events(&mut self, template: &TemplateRef) -> DomainResult<Vec<TemplateEvent>>;
```
`update_entry` also writes `answer_seq`. In-memory store mirrors Postgres semantics,
including: a template's definition and identity never change (`insert_template` of an
existing ref is `Duplicate`), and `set_template_status` refuses any transition other than
pending→approved, pending→rejected, approved→revoked with `DomainError::Store`.

- [ ] **Step 1:** failing contract cases in `contract/templates.rs` (registered in
  `contract/mod.rs`): insert/get round-trip; duplicate insert; illegal transitions refused;
  change numbers increase; `templates_changed_since` filters and orders and limits;
  `claim_publisher` first owner wins; events list in order; `is_agent` after
  `record_agent_owner`; entry `answer_seq` round-trips through `update_entry`.
- [ ] **Step 2–4:** `cargo test -p push-testkit --locked -j 4` red → implement memory →
  green (Postgres runs in Task 5). Workspace builds; fmt; clippy.
- [ ] **Step 5: Commit** `feat(domain): template registry storage contract`.

### Task 5: Postgres (push-store)

**Files:** create `crates/push-store/migrations/0005_templates.sql`; modify
`crates/push-store/src/{store,rows,codes}.rs`; `crates/push-store/tests/` (schema tests in
an existing test file if one checks constraints, else `tests/templates.rs`).

```sql
-- Rich message templates (REQUEST PUSH feature 8, spec 2026-10-26 rich message templates).
--   message.content_type  3 rich, 4 answer
--   template_version.status 1 pending, 2 approved, 3 rejected, 4 revoked
ALTER TABLE messaging.message DROP CONSTRAINT message_content_type_check;
ALTER TABLE messaging.message ADD CONSTRAINT message_content_type_check
    CHECK (content_type BETWEEN 1 AND 4);
-- Cards: the seq of the first valid answer.
ALTER TABLE messaging.message ADD COLUMN answer_seq bigint CHECK (answer_seq >= 1);

CREATE TABLE messaging.template_publisher (
    publisher     text PRIMARY KEY CHECK (publisher ~ '^[a-z][a-z0-9-]{0,31}$'),
    -- NULL: the server's own (built-ins).
    owner_user_id uuid REFERENCES messaging.known_user,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE messaging.template_version (
    publisher    text NOT NULL REFERENCES messaging.template_publisher,
    name         text NOT NULL CHECK (name ~ '^[a-z][a-z0-9-]{0,31}$'),
    version      integer NOT NULL CHECK (version BETWEEN 1 AND 65535),
    title        text NOT NULL CHECK (length(title) BETWEEN 1 AND 80),
    definition   text NOT NULL CHECK (octet_length(definition) <= 16384),
    status       smallint NOT NULL CHECK (status BETWEEN 1 AND 4),
    submitted_by uuid REFERENCES messaging.known_user,
    submitted_at timestamptz NOT NULL,
    reviewed_at  timestamptz,
    change_seq   bigint UNIQUE,
    PRIMARY KEY (publisher, name, version),
    -- A seeded built-in is reviewed (approved) when it is inserted.
    CHECK ((status = 1) = (reviewed_at IS NULL)),
    CHECK ((status IN (2, 4)) = (change_seq IS NOT NULL))
);
CREATE INDEX template_version_status ON messaging.template_version (status, submitted_at);
CREATE INDEX template_version_pending_by ON messaging.template_version (submitted_by)
    WHERE status = 1;
-- Immutable identity and definition; one-way status.
CREATE FUNCTION messaging.template_version_guard() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'template versions are never deleted' USING ERRCODE = '23514';
    END IF;
    IF (OLD.publisher, OLD.name, OLD.version, OLD.title, OLD.definition, OLD.submitted_by,
        OLD.submitted_at) IS DISTINCT FROM (NEW.publisher, NEW.name, NEW.version, NEW.title,
        NEW.definition, NEW.submitted_by, NEW.submitted_at) THEN
        RAISE EXCEPTION 'an approved or submitted template version is immutable'
            USING ERRCODE = '23514';
    END IF;
    IF NOT ((OLD.status = 1 AND NEW.status IN (2, 3)) OR (OLD.status = 2 AND NEW.status = 4)
            OR OLD.status = NEW.status AND OLD.change_seq IS NOT DISTINCT FROM NEW.change_seq
               AND OLD.reviewed_at IS NOT DISTINCT FROM NEW.reviewed_at) THEN
        RAISE EXCEPTION 'template status % cannot become %', OLD.status, NEW.status
            USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER template_version_guard BEFORE UPDATE OR DELETE ON messaging.template_version
    FOR EACH ROW EXECUTE FUNCTION messaging.template_version_guard();

-- One row: the last change number handed out (approve, revoke, seed).
CREATE TABLE messaging.template_change (
    id       boolean PRIMARY KEY DEFAULT true CHECK (id),
    last_seq bigint NOT NULL DEFAULT 0 CHECK (last_seq >= 0)
);
INSERT INTO messaging.template_change DEFAULT VALUES;

-- Append-only audit trail.
CREATE TABLE messaging.template_event (
    event_id  bigserial PRIMARY KEY,
    publisher text NOT NULL,
    name      text NOT NULL,
    version   integer NOT NULL,
    action    text NOT NULL CHECK (action IN ('submit', 'approve', 'reject', 'revoke', 'seed')),
    actor     uuid,
    reviewer  text CHECK (length(reviewer) BETWEEN 1 AND 64),
    note      text CHECK (length(note) <= 500),
    at        timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (publisher, name, version) REFERENCES messaging.template_version
);
CREATE INDEX template_event_version ON messaging.template_event (publisher, name, version, event_id);
CREATE FUNCTION messaging.template_event_append_only() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    RAISE EXCEPTION 'the template audit trail is append-only' USING ERRCODE = '23514';
END
$$;
CREATE TRIGGER template_event_append_only BEFORE UPDATE OR DELETE ON messaging.template_event
    FOR EACH ROW EXECUTE FUNCTION messaging.template_event_append_only();
```
(Implementer: verify the existing content_type CHECK constraint name with
`\d messaging.message` semantics — Postgres names an inline column check
`message_content_type_check`; confirm by a schema test. Fix the boolean precedence in the
guard with explicit parentheses.) `codes.rs`: ContentType 3/4, TemplateStatus 1..4.
Trigger violations map to `DomainError::Store` as other check violations do.

- [ ] Steps: add migration; `cargo clean -p push-store`; implement; run
  `cargo test -p push-store --locked -j 4` (the behaviour suite incl. Task 4 contract
  cases, plus schema tests: direct SQL UPDATE of an approved definition refused, DELETE
  refused, approved→pending refused, event UPDATE refused); fmt; clippy; commit
  `feat(store): store templates, card state and the audit trail in Postgres`.

### Task 6: Registry operations (push-domain) + scenarios

**Files:** create `crates/push-domain/src/registry.rs`, `crates/push-testkit/src/scenarios/templates.rs`.

```rust
pub struct ReviewCommand { pub template: TemplateRef, pub action: ReviewAction,
    pub reviewer: String, pub note: Option<String> }
pub enum ReviewAction { Approve, Reject, Revoke }
pub struct Catalog { pub seq: i64, pub more: bool, pub entries: Vec<StoredTemplate> }
pub const MAX_PENDING_PER_USER: u32 = 20;
pub const MAX_CATALOG_PAGE: u32 = 200;
impl Messaging {
  /// Parses (Definition::parse), refuses reserved publishers (`TemplateConflict`), checks
  /// the user exists (UnknownUser), claims the publisher for the user (another owner →
  /// TemplateConflict("the publisher belongs to another account")), refuses over
  /// MAX_PENDING_PER_USER, inserts pending with canonical JSON (Duplicate →
  /// TemplateExists), audits `submit`.
  pub async fn submit_template(&self, user: UserId, definition: &[u8]) -> DomainResult<StoredTemplate>;
  /// reviewer 1..=64 chars, no control chars; note ≤ 500 chars, no control chars except \n.
  /// Locks the row (TemplateNotFound); approve/reject need pending, revoke needs approved
  /// (else TemplateConflict("the template is <status>")); approve/revoke take
  /// next_template_change; audits.
  pub async fn review_template(&self, command: ReviewCommand) -> DomainResult<StoredTemplate>;
  /// since < 0 → Invalid; limit 0 or > MAX → MAX. seq = last entry's change_seq when the
  /// page is full (more = true), else max(since, last_template_change).
  pub async fn template_catalog(&self, since: i64, limit: u32) -> DomainResult<Catalog>;
  pub async fn list_templates(&self, status: Option<TemplateStatus>, limit: u32) -> DomainResult<Vec<StoredTemplate>>;
  pub async fn template_detail(&self, template: &TemplateRef) -> DomainResult<(StoredTemplate, Vec<TemplateEvent>)>;
  /// Inserts each built-in that is missing as approved (claiming `agent` for the server),
  /// with a change number and a `seed` event; leaves existing rows (even revoked) alone.
  /// Returns how many were inserted. Idempotent and safe to run concurrently.
  pub async fn seed_builtin_templates(&self) -> DomainResult<usize>;
}
```
Scenarios: submit → pending (not in catalog); approve → in catalog with definition; reject
→ never in catalog; revoke → catalog lists `revoked` at a higher seq; catalog since/limit
paging; second account under the same publisher refused; reserved publisher refused;
invalid definition refused with `Template`; 21st pending refused; approving twice refused;
revoking pending refused; seeding twice inserts once and keeps a revoked built-in revoked;
events record submit/approve/reject/revoke with actor/reviewer/note.

- [ ] Steps: red (in-memory: `cargo test -p push-testkit --locked -j 4`), implement, green;
  then `cargo test -p push-store --locked -j 4`; fmt; clippy; commit
  `feat(domain): submit, review and list templates`.

### Task 7: Sending and editing RICH and ANSWER; card state (push-domain) + scenarios

**Files:** create `crates/push-domain/src/rich.rs`, `crates/push-testkit/src/scenarios/rich.rs`;
modify `send.rs`, `edit.rs`, `lib.rs`, `crates/push-testkit/src/world.rs` (helpers).

```rust
/// A message to append; `send_text` becomes a wrapper with ContentType::Text.
pub struct SendMessage { pub sender: UserId, pub device: DeviceId, pub conversation: ConversationId,
    pub client_msg_id: ClientMsgId, pub content_type: ContentType, pub body: Vec<u8>,
    pub client_ts: Option<DateTime<Utc>> }
impl Messaging { pub async fn send_message(&self, command: SendMessage) -> DomainResult<SendReceipt>; }
```
Rules (pure checks before the transaction, store checks inside it after `prepare_append`,
so retries keep returning the original receipt first):
- Text: `validate_text`. SystemEvent/other: `Invalid`.
- Rich: `RichBody::parse` (→ `Template`); in tx: `template()` must exist and be Approved
  (else `Template("template <ref> is not approved")`, same message for missing, pending,
  rejected, revoked); `Definition::parse(stored.definition)`; `body.check`; sender kind
  `is_agent` → Agent else Human; `allows(kind)` else `Template("template <ref> cannot be
  sent by a <kind>")`.
- Answer: `AnswerBody::parse`; in tx: card by message id in the same conversation, kind
  Message, `seq >= member.visible_from_seq`, not deleted (else `TargetNotFound`); card
  content type Rich (else `Template("the target is not a card")`); card sender ≠ answerer (else
  `Template("you cannot answer your own card")`); `answer_seq` none (else `CardAnswered`);
  card's template approved and parsed; the card body parsed; `AnswerBody::check`. The new
  entry has `target_seq = Some(card.seq)`; after insert, `update_entry(card with
  answer_seq = Some(seq))`.
- Edit (`edit_text`): after resolving the target: Text target → `validate_text`; Rich
  target → full RICH checks as for a send by the editor, and an answered card →
  `CardAnswered`; Answer target → `Template("an answer cannot be edited")`; the edit entry
  takes the target's content type. Note `edit_text` currently validates text before the
  transaction: move the body check to after the target is known (keep empty/oversize body
  refusal first: body empty or > MAX_RICH_BYTES).
- Delete unchanged; deleting an answer leaves the card answered.

Scenarios (in-memory + Postgres via the suite; use `seed_builtin_templates` and
`record_agent_owner` through `world.svc.enroll_agent` to make an agent): agent sends
valid approval → stored with content type Rich; human sends agent-only approval → refused
(`Template`); unapproved (pending) / rejected / revoked / unknown template → refused;
invalid params → refused; a human-allowed submitted+approved template sent by a human →
ok; answer approve → card `answer_seq` set, answer `target_seq` = card seq; second answer →
`CardAnswered`; answer by the card's sender → refused; answer referencing a card in another
conversation → `TargetNotFound`; answer to a TEXT message → refused; bad value → refused;
answer after the template is revoked → refused; edit card valid → ok; edit card with bad
params → refused; edit answered card → `CardAnswered`; edit answer → refused; retry of a
RICH send after revocation returns the original receipt.

- [ ] Steps: red, implement, green (testkit then push-store); fmt; clippy; commit
  `feat(domain): enforce templates on send and edit; answer cards once`.

### Task 8: message-svc

**Files:** `crates/push-message-svc/src/{grpc,convert,server,requests}.rs`, new
`src/templates.rs` (the `Templates` gRPC service), tests in `tests/service.rs` or module tests.

- `Messaging::send` accepts TEXT, RICH, ANSWER → `send_message`; others `invalid(...)`.
- `deliver()` maps `Rich`/`Answer` and `answer_seq`.
- `domain_error`: `Template` → (InvalidArgument, Invalid); `CardAnswered` →
  (FailedPrecondition, Rejected); `TemplateNotFound` → (NotFound, Rejected);
  `TemplateExists` → (AlreadyExists, Rejected); `TemplateConflict` → (FailedPrecondition,
  Rejected), so auth-svc maps them to 404 / 409.
- `TemplatesApi` implements `templates_server::Templates`, wrapped with
  `InternalToken::directory(...)` (same token as `Directory`). Converts statuses, times in
  micros, ids as 16 bytes; `Get` returns `outline(definition)` and events.
- Startup (`server.rs`): after migrations and before serving, `seed_builtin_templates()`
  with the same retry as other startup steps; log `templates.seeded` (security target,
  count) when > 0.
- Security log lines (target `security`) for submit/approve/reject/revoke: event names
  `templates.submitted|approved|rejected|revoked` with ref, actor/reviewer; never bodies.
- [ ] Tests: gRPC Send RICH ok / refused INVALID; Templates without the token refused;
  Submit → Review(approve) → Catalog contains it; seeding at startup visible in Catalog.
- [ ] fmt; clippy; `cargo test -p push-message-svc --locked -j 4`; commit
  `feat(message-svc): accept rich and answer messages and serve the Templates API`.

### Task 9: auth-svc HTTP

**Files:** create `crates/push-auth-core/src/templates.rs` (trait `TemplateRegistry`, gRPC
client, `NoRegistry`); modify `api.rs` (routes), `admin.rs` (routes), `limits.rs`
(`Action::TemplateSubmit`: 10/hour per user), `server.rs` (wire the client from
`PUSH_MESSAGE_SVC_URL` + `PUSH_DIRECTORY_TOKEN`, like `GrpcDirectory`), `audit.rs` (events
for the new routes), tests in `src/tests/templates.rs` with a fake registry.

- `POST /v1/templates` (access token; body ≤ 16 KiB, `application/json`) → 201
  `{template, status, title, submitted_at}`; errors `invalid` 400 (with message), 409
  `conflict`, 429, 503.
- `GET /v1/templates?since=N` (access token; N ≥ 0 default 0) → 200
  `{"seq": n, "more": bool, "templates": [{"template": ref, "status": "approved"|"revoked",
  "seq": n, "definition": {...}?}]}`, `ETag: "tpl-<since>-<seq>"`, `Cache-Control: private,
  no-cache`; `If-None-Match` equal (or `*`) → 304 with the ETag and no body.
- Admin listener (admin token, same `authorized` and limits as `revoke-sessions`):
  `GET /v1/admin/templates?status=` → `{"templates":[summary...]}`;
  `GET /v1/admin/templates/{ref}` → `{summary..., definition, outline, events}`;
  `POST /v1/admin/templates/{ref}/approve|reject|revoke` `{"reviewer", "note"?}` → summary.
  Log `auth.admin.template_reviewed` (security) with ref, action, reviewer, ip.
- gRPC status → AuthError: InvalidArgument→Invalid(400), NotFound→404 `not_found`,
  AlreadyExists/FailedPrecondition→409 `conflict`, else Unavailable(503). Check the existing
  `AuthError` variants; add `NotFound`/`Conflict` only if missing, with codes.
- [ ] Tests (fake registry): submit happy path and 400 pass-through; catalog ETag + 304;
  admin routes need the token (401 without); approve path; bad ref 400.
- [ ] fmt; clippy; `cargo test -p push-auth-core --locked -j 4`; commit
  `feat(auth): serve template submission, the catalog and admin review`.

### Task 10: Whole-stack acceptance (push-smoke)

**Files:** create `crates/push-smoke/tests/templates.rs` (reuse `tests/common`).
Flow: start stack; human owner + owned agent (as `tests/owned_agents.rs` does) and a
human peer who are contacts with the agent; agent sends `agent.approval@1` RICH → SendAck;
human answers → SendAck; second answer → REJECTED; human sends `agent.approval@1` →
INVALID; owner submits `acme.notice@1` (senders human) via `POST /v1/templates` → 201;
sending it → INVALID (pending); admin approves via the admin listener → 200; `GET
/v1/templates?since=0` lists it (and the four built-ins), ETag replay → 304; sending it →
SendAck; submit with a URL / unknown block / 17 params → 400; re-submitting the approved ref
→ 409 (immutable); revoke → catalog shows revoked; sending → INVALID.
- [ ] Run `cargo test -p push-smoke --test templates --locked -j 4`; fmt; clippy; commit
  `test(smoke): rich templates through the whole stack`.

### Task 11: SDK and bindings

**Files:** `crates/push-sdk/src/{model,store,client/sending,store/outbox,engine,testing}.rs`,
`bindings/ffi/src/json.rs`, `bindings/c/src/client.rs`, `bindings/c/include/push_sdk.h`,
`bindings/node/src/client.rs`, `bindings/node/index.d.ts`, `bindings/node/test/*`.

- `MessageKind::{Rich, Answer}`; store codes 3/4 (`content_type_code`, readback);
  `Message.answer_seq: Option<i64>` (new nullable column via the SDK's schema migration
  mechanism — follow how the SDK store versions its schema); an incoming ANSWER entry with
  `target_seq` sets the card's `answer_seq` (if not set).
- `Client::send_rich(conversation, body_json: &str)` and `send_answer(...)`: check with
  `RichBody::parse` / `AnswerBody::parse` (`SdkError::Invalid`), queue with the content type
  (outbox stores it already). Edits of Answer messages refused; edits of Rich take a RICH
  body (`edit` checks the body by the target's kind).
- Bindings: JSON kind strings `"rich"`, `"answer"`, field `answerSeq`; C
  `push_client_send_rich`/`push_client_send_answer` (header regenerated/updated so
  `tests/header.rs` passes); node `sendRich`, `sendAnswer` + `index.d.ts` + a test.
- [ ] `cargo test -p push-sdk --locked -j 4`; `cargo test --locked -p push-sdk-c -p
  push-sdk-ffi`; in `bindings/node`: `npm test` and `npm run typecheck`; fmt; clippy;
  commit `feat(sdk): rich cards and answers in the SDK and bindings`.

### Task 12: Documentation

**Files:** create `docs/rich-messages.md`; modify `CHANGELOG.md` (Unreleased → Added),
`README.md` (link), `docs/sdk-bindings.md` (new calls).
`docs/rich-messages.md`: overview and enforcement model (D1: server sees bodies; what is
enforced vs client duties), tpl/1 reference (every field, type, block, cap, wording rule),
RICH and ANSWER bodies, card state, the four built-ins verbatim with a note that they were
designed upstream and the consumer should reconcile with its spec, the review checklist,
the admin API with curl examples, the client fetch protocol (since/seq/more/ETag/304,
revoked handling), publishing, audit events, limits, follow-ups.
- [ ] Commit `docs: rich message templates, review checklist and catalog protocol`.

---

## Self-review

Spec coverage: D1 → Task 12 docs + Task 7 full enforcement; D2 → Task 5; D3 → Tasks 1-2,
11; D4 → Tasks 3, 8, 9; D5 → Task 9; D6 → Task 6; D7 → Tasks 5-6; D8 → Tasks 6, 9; D9 →
Task 7; D10 → Tasks 3, 7, 11; D11 → Tasks 7, 10, 12; D12 → Tasks 2, 6, 8; D13 → Tasks 5, 6,
8, 9. Acceptance tests → Tasks 1, 2, 7, 10.
