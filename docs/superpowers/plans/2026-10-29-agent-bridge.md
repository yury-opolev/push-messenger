# Agent bridge with a Coda backend — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking. Every task is TDD: write the listed tests first, see
> them fail, implement, see them pass, then commit.

**Goal:** Reach a local AI coding agent (Coda first) from push-messenger as a private agent
contact: agent-key sign-in in push-sdk through an app-supplied signer, a backend-independent
bridge core, a Coda backend driving `coda serve`, two new built-in templates, docs and release
notes.

**Architecture:** `push-sdk` gains agent-key enrollment/sign-in (`AgentSigner` trait; the SDK
builds the claims, the signer returns the compact JWS) and a `CardBook` that checks card
answers. A new crate `push-agent-key` holds the signers: a Windows CNG (NCrypt) signer with a
non-exportable P-256 key (TPM provider first, software KSP fallback) and a portable software
signer for tests/dev. A new crate `push-bridge` (binary `push-agent-bridge`) holds the shared
core (owner gate, rendering, cards, commands, activity, masking, state/config, logging,
lifecycle) driving a small `Backend` trait; `CodaBackend` speaks JSON-RPC 2.0 with LSP
Content-Length framing to `coda serve` over stdio; `TestBackend` proves the core runs without
Coda.

**Tech Stack:** Rust 2024, MSRV 1.85 (no let-chains; check any std API you use is stable in
1.85), tokio, async-trait, serde/serde_json, regex, ring (software ECDSA),
windows-sys 0.61 (`cfg(windows)` only), tracing + tracing-subscriber (JSON lines).

**Source request:** `C:\Users\yurio\.copilot\session-state\f24a938b-b716-4b96-9fa9-9acb077bec41\files\push-bridge-request.md`
(read it — it is the spec). Coda protocol: `C:\Users\yurio\Documents\github\coda-cli\docs\serve-protocol.md`,
`docs\protocol\catalog.md`, `docs\protocol\catalog.json`, `docs\protocol\schemas\` (read-only).

---

## Global rules (apply to every task)

- Work in `C:\Users\yurio\Documents\github\push-agent-bridge` (git worktree, branch
  `feat/agent-bridge`). Never touch `C:\Users\yurio\Documents\github\push-entra-1a164092` or any
  `.github/copilot/` folder.
- Windows host, PowerShell. CI runs on Linux: everything must build and test on Linux too.
  Windows-only code lives behind `#[cfg(windows)]`.
- CI gates (all must pass): `cargo fmt --all -- --check`;
  `cargo clippy --workspace --all-targets --locked -- -D warnings`; `cargo test --workspace --locked`
  (Docker is available for DB tests); MSRV job `cargo +1.85 build --workspace --locked`;
  `cargo deny check` (licences MIT/Apache/BSD/ISC/Zlib/Unicode-3.0/BSL; no advisories).
  **No let-chains** (`if let … && …`) — MSRV 1.85. `--locked`: after adding a dependency run a
  normal `cargo build` once so Cargo.lock updates, and commit Cargo.lock.
- New crates are workspace members (root `Cargo.toml` `members`), use `version.workspace = true`
  etc., and `[lints] workspace = true` (which forbids `unsafe_code`) — except `push-agent-key`
  which needs `unsafe` for NCrypt (see Task 4).
- Conventional commits; every commit message ends with a blank line and
  `Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>`.
- Style: follow the repo's doc-comment style (short, precise `///` docs; plain English). Only
  comment code that needs clarification.
- Never log or `Debug`-print tokens, assertions, message bodies, previews or plans.
- Character counts are Unicode scalar values (`chars()`), never bytes, when the spec says
  "characters".

## File structure

```
crates/push-domain/src/templates/builtin/agent.choose.json          (new, Task 1)
crates/push-domain/src/templates/builtin/agent.plan-approval.json   (new, Task 1)
crates/push-domain/src/templates/builtin.rs                         (Task 1)
crates/push-auth-core/src/agent_key/mod.rs                           (Task 2: testing-only verifier export)
crates/push-sdk/src/agent_key.rs                                     (new, Task 2)
crates/push-sdk/src/auth.rs, client.rs, error.rs, lib.rs, testing.rs (Task 2)
crates/push-sdk/src/cards.rs                                         (new, Task 3)
crates/push-agent-key/{Cargo.toml, src/lib.rs, src/software.rs, src/cng.rs}   (new, Task 4)
crates/push-bridge/Cargo.toml                                        (new, Task 5)
crates/push-bridge/src/lib.rs
crates/push-bridge/src/mask.rs        masking + char caps                          (Task 5)
crates/push-bridge/src/commands.rs    owner command parsing + /help text           (Task 5)
crates/push-bridge/src/folders.rs     /cd real-path-inside-roots check             (Task 5)
crates/push-bridge/src/config.rs      config.json                                  (Task 5)
crates/push-bridge/src/state.rs       state.json + BridgeSecretStore               (Task 5)
crates/push-bridge/src/backend.rs     Backend trait, events, capabilities          (Task 5)
crates/push-bridge/src/backoff.rs     restart backoff                              (Task 5)
crates/push-bridge/src/render.rs      TurnRenderer (agent.reply@1)                 (Task 6)
crates/push-bridge/src/cards.rs       request → card, answer → backend answer      (Task 6)
crates/push-bridge/src/coda/{mod.rs, framing.rs, rpc.rs, backend.rs}               (Task 7)
crates/push-bridge/src/messenger.rs   Messenger trait, SdkMessenger, TestMessenger (Task 8)
crates/push-bridge/src/test_backend.rs TestBackend                                (Task 8)
crates/push-bridge/src/core.rs (+ core/ submodules if large)                       (Task 8)
crates/push-bridge/tests/bridge.rs    integration tests                            (Task 8)
crates/push-bridge/src/main.rs, src/cli.rs, src/logging.rs, src/signer.rs          (Task 9)
crates/push-bridge/scripts/install.ps1, uninstall.ps1                              (Task 9)
docs/agent-bridge.md, docs/agent-key-sign-in.md, docs/rich-messages.md, docs/sdk-bindings.md?, README.md, CHANGELOG.md (Task 10)
```

---

### Task 1: Built-in templates `agent.choose@1` and `agent.plan-approval@1`

Built-ins are JSON files compiled into `push-domain` (`templates/builtin.rs`, `SOURCES`) and
seeded at start by message-svc (`push-message-svc/src/templates.rs::seed_builtins`), idempotent.
No migration is needed.

**Files:**
- Create: `crates/push-domain/src/templates/builtin/agent.choose.json`,
  `crates/push-domain/src/templates/builtin/agent.plan-approval.json`
- Modify: `crates/push-domain/src/templates/builtin.rs` (SOURCES to 6, docs, tests)
- Modify: every test/scenario that counts or lists the built-ins:
  `grep -rn "agent.plan@1\|seed_builtin_templates().await, Ok(4)\|catalog.seq, 4" crates`
  (at least `crates/push-testkit/src/scenarios/templates.rs`, `crates/push-domain/src/templates/outline.rs`
  tests, `crates/push-domain/src/registry.rs`, push-message-svc tests). Check the name
  `plan-approval` is a valid template name (hyphen) per `definition.rs` name rules; if hyphens
  are not allowed, use `plan_approval` and note it.

agent.choose.json (publisher `agent`):
```json
{
  "format": "tpl/1",
  "publisher": "agent",
  "name": "choose",
  "version": 1,
  "title": "Multiple choice",
  "senders": ["agent"],
  "params": {
    "question": {"type": "string", "max_len": 1000, "multiline": true},
    "options": {"type": "list", "item": {"type": "option"}, "max_items": 10}
  },
  "layout": [
    {"type": "text", "param": "question"},
    {"type": "multi", "label": "Choose any", "param": "options", "min": 1, "max": 10}
  ]
}
```
agent.plan-approval.json:
```json
{
  "format": "tpl/1",
  "publisher": "agent",
  "name": "plan-approval",
  "version": 1,
  "title": "Plan approval",
  "senders": ["agent"],
  "params": {
    "title": {"type": "string", "max_len": 120},
    "plan": {"type": "string", "max_len": 4000, "multiline": true}
  },
  "layout": [
    {"type": "heading", "param": "title"},
    {"type": "preview", "param": "plan", "label": "Plan"},
    {"type": "buttons", "options": [
      {"id": "approve", "label": "Approve plan", "style": "primary"},
      {"id": "reject", "label": "Reject", "style": "danger"}
    ]}
  ]
}
```
Match the formatting of the existing JSON files.

- [ ] Step 1: Update `builtin.rs` tests to expect six refs (`…, "agent.plan@1", "agent.choose@1",
  "agent.plan-approval@1"`) and titles (`("Multiple choice", true)`, `("Plan approval", true)`);
  add a body test in `body.rs` tests: a choose card with 3 options checks OK, `offered` returns the
  option ids, an answer with two ids checks OK against it; a plan-approval card offers
  `["approve","reject"]`. Run `cargo test -p push-domain` → FAIL.
- [ ] Step 2: Add the JSON files and SOURCES entries. Run → PASS.
- [ ] Step 3: Update the seeding scenario counts (Ok(4)→Ok(6), seq 4→6, ref lists) and any other
  count. Run `cargo test -p push-domain -p push-testkit -p push-message-svc` (Docker needed) → PASS.
- [ ] Step 4: Commit `feat(domain): built-in agent.choose@1 and agent.plan-approval@1 templates`.

Docs for these templates are written in Task 10.

---

### Task 2: push-sdk agent-key enrollment and sign-in through a signer

Read `docs/agent-key-sign-in.md` (assertion format) and `crates/push-sdk/src/{auth.rs,client.rs,error.rs,testing.rs}`.

**Public API (new module `crates/push-sdk/src/agent_key.rs`, re-exported from lib.rs):**
```rust
/// How long the assertions the SDK asks for live: two minutes (auth-svc allows at most 300 s).
pub const AGENT_ASSERTION_LIFETIME_S: i64 = 120;

/// The claims of one agent-key assertion. The SDK builds them, with a new `jti` every time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentClaims { pub aud: String, pub iat: i64, pub exp: i64, pub jti: String }
impl AgentClaims {
    /// `iat = now_s`, `exp = now_s + AGENT_ASSERTION_LIFETIME_S`, `jti` = 32 lowercase hex chars from a v4 UUID (simple form).
    pub fn new(audience: &str, now_s: i64) -> Self;
    /// The claims as compact JSON: {"aud":…,"iat":…,"exp":…,"jti":…}.
    pub fn to_json(&self) -> String;
}

/// A failure of the app's signer: `code` is the key provider's code (e.g. `NTE_BAD_KEYSET`,
/// `0x80090016`), never key material.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct SignerError { pub code: String, pub message: String }

/// Signs agent-key assertions with a P-256 private key the app keeps (ideally non-exportable,
/// in OS key storage). Called on a blocking thread.
pub trait AgentSigner: Send + Sync {
    /// The compact JWS (`header.payload.signature`) for `claims`, as docs/agent-key-sign-in.md specifies.
    fn sign(&self, claims: &AgentClaims) -> Result<String, SignerError>;
}

/// A P-256 public key: the 32-byte coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentPublicKey { pub x: [u8; 32], pub y: [u8; 32] }
impl AgentPublicKey {
    /// RFC 7638 thumbprint (base64url SHA-256 of {"crv":"P-256","kty":"EC","x":…,"y":…}).
    pub fn thumbprint(&self) -> String;
}

/// Builds a compact JWS for signers: the header {"alg":"ES256","typ":"push-agent+jwt","jwk":{kty,crv,x,y}},
/// the claims, and `sign` called over the ASCII signing input, returning the 64-byte r||s.
pub fn assemble_assertion(
    key: &AgentPublicKey,
    claims: &AgentClaims,
    sign: impl FnOnce(&[u8]) -> Result<[u8; 64], SignerError>,
) -> Result<String, SignerError>;
```
Add `base64` and `sha2` (workspace) as normal deps of push-sdk (sha2 is already a build-dep).

**SdkError:** add variant `Signer(String)` (`#[error("signer: {0}")]`, `code()` = `"signer"`,
not retryable). Its string is `SignerError`'s Display. Check bindings/ffi and bindings/c/node for
exhaustive matches on SdkError (it is `#[non_exhaustive]`) and for lists of error codes in docs
(`docs/sdk-bindings.md`) and add `signer` where codes are enumerated.

**AuthApi (auth.rs):** `agent_key(token, enrollment_code: Option<&str>) -> Result<String, SdkError>`
(registration token; body `{token}` or `{token, enrollment_code}`) and
`agent_key_device(token, device) -> Result<Tokens, SdkError>` (`{token, device_id}`); both `once`
(never repeated: each assertion is single-use).

**Client (client.rs):** extend `Proof` with
`AgentKey { signer: Arc<dyn AgentSigner>, audience: String, enrollment_code: Option<String> }`.
Each `device()` / `registration()` call mints a **new** assertion: `AgentClaims::new(&audience, unix_now())`,
then `tokio::task::spawn_blocking(move || signer.sign(&claims))`; a `SignerError` becomes
`SdkError::Signer`. For an enrollment (`enrollment_code: Some`), skip the device path entirely
(forget any last device first: an enrollment creates the account and needs a new device).
New public methods (doc them like `sign_in_with_entra`):
```rust
/// Enrolls this agent with the owner's one-time code and registers this device (docs/agent-key-sign-in.md).
/// `audience`: auth-svc's PUSH_AUTH_AGENT_KEY_AUDIENCE; `None` = `Config::auth_url` without a trailing `/`.
pub async fn enroll_with_agent_key(&self, signer: Arc<dyn AgentSigner>, audience: Option<&str>,
    enrollment_code: &str, device_name: &str, platform: Platform) -> Result<Account, SdkError>;
/// Signs the enrolled agent in: the device it registered before, or a new one.
pub async fn sign_in_with_agent_key(&self, signer: Arc<dyn AgentSigner>, audience: Option<&str>,
    device_name: &str, platform: Platform) -> Result<Account, SdkError>;
```
Store the auth URL in `Inner` if needed to compute the default audience.

**Fake auth (testing.rs `FakeAuth`):** add route `POST /v1/auth/agent-key` that records each JSON
body in `pub agent_key_bodies: Arc<Mutex<Vec<Value>>>` and decodes the token's payload
(base64url middle part) to check `exp - iat <= 300`, `aud` equals a configured audience
(`FakeAuth.url` trimmed), and refuses a reused `jti` with 401 `token_invalid`
(keep `pub agent_key_jtis: Arc<Mutex<Vec<String>>>`). `{token, enrollment_code}` with code
`"BADCODE"` → 400 `enrollment_invalid`; `{token, enrollment_code}` ok → `{user_id, registration_token, expires_in_s}`;
`{token}` → same; `{token, device_id}` → tokens (or 404 `device_not_found` for an unknown device), as
the Entra route does. Mirror the existing entra route code.

**Tests (client.rs tests module, alongside the entra test):**
1. `an_agent_key_enrollment_registers_and_a_later_sign_in_reuses_the_device`: a test signer
   (struct holding a counter, returning `"h.<base64url(claims json)>.s"` — the fake only reads the
   payload) → enroll → account; bodies[0] == `{token, enrollment_code}`; sign in again → bodies[1] has
   `device_id`; every recorded jti distinct, `exp - iat == 120`, `aud == auth url`.
2. `every_agent_key_request_signs_a_new_assertion`: sign-in where the stored device is unknown
   (device_not_found path then registration) → two requests, two distinct jtis, signer called twice.
3. `a_failing_signer_stops_the_sign_in_with_its_code`: signer returns
   `SignerError{code:"NTE_BAD_KEYSET",..}` → `SdkError::Signer`, `code()=="signer"`, message contains
   `NTE_BAD_KEYSET`, and no request reached the fake.
4. `a_spent_code_is_enrollment_invalid`.
5. Unit tests in agent_key.rs: `AgentClaims::new` jti 32 hex and fresh per call; `to_json` exact;
   `thumbprint` of the P-256 base point equals `xx0BcA-wMohw8atYDJOe6peGModklG2wRHBlXHMvl0M`
   (x = `axfR8uEsQkf4vOblY6RA8ncDfYEt6zOg9KE5RdiYwpY`, y = `T-NC4v4af5uO5-tKfA-eFivOM1drMV7Oy7ZAaDe_UfU`);
   `assemble_assertion` produces three base64url parts, header JSON exactly members alg/typ/jwk.

**push-auth-core verifier export** (used by Task 4's cross-check): in
`crates/push-auth-core/src/agent_key/mod.rs` add
```rust
/// The server's assertion check, for other crates' tests: the key's thumbprint, or the refusal reason.
#[cfg(feature = "testing")]
pub fn verify_assertion(token: &str, audience: &str, now: i64) -> Result<String, &'static str> {
    jws::verify(token, audience, now).map(|assertion| assertion.thumbprint)
}
```
(the `testing` feature already exists.)

- [ ] Steps: tests first (fail), implement, `cargo test -p push-sdk`, `cargo clippy -p push-sdk --all-targets -- -D warnings`,
  `cargo test -p push-sdk-c -p push-sdk-ffi` (bindings still build), commit
  `feat(sdk): agent-key enrollment and sign-in through an app signer`.

---

### Task 3: push-sdk `CardBook` (answer checks)

The server does not check who a card was meant for; the card's sender must check every answer.

**New module `crates/push-sdk/src/cards.rs`, re-export `CardBook, OpenCard, CardCheck, CardRefusal`:**
```rust
/// A card this client sent and still waits on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCard {
    pub conversation: ConversationId,
    pub client_msg_id: ClientMsgId,
    /// The server's id, once the card was stored (`Event::MessageSent`).
    pub message_id: Option<MessageId>,
    /// The only user whose answer counts.
    pub owner: UserId,
    /// The values the card offers (`RichBody::offered`).
    pub offered: Vec<String>,
    /// At most this many values in one answer (1 for buttons/single choice).
    pub max_values: usize,
    /// Unix ms after which answers no longer count.
    pub deadline_ms: i64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardRefusal { NotACardAnswer, UnknownCard, NotOwner, NotOffered, Late }
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardCheck {
    /// The owner's answer: the card is closed and removed from the book.
    Accepted { card: OpenCard, values: Vec<String> },
    Refused(CardRefusal),
}
#[derive(Debug, Default)]
pub struct CardBook { /* cards keyed by ClientMsgId */ }
impl CardBook {
    pub fn new() -> Self;
    pub fn open(&mut self, card: OpenCard);
    /// Records the server id from `Event::MessageSent` for a card in the book (no-op otherwise).
    pub fn sent(&mut self, message: &Message);
    /// Checks a delivered message that may answer one of the cards. Accepted only when it is an
    /// ANSWER in the card's conversation, from the owner, naming the card's server id, with 1..=max_values
    /// distinct values all offered, received at or before the deadline (`now_ms`).
    pub fn check(&mut self, message: &Message, now_ms: i64) -> CardCheck;
    /// Removes a card (answered another way, declined, or refused by the server).
    pub fn close(&mut self, client_msg_id: ClientMsgId) -> Option<OpenCard>;
    /// Removes and returns every card past its deadline.
    pub fn expire(&mut self, now_ms: i64) -> Vec<OpenCard>;
    pub fn get(&self, client_msg_id: ClientMsgId) -> Option<&OpenCard>;
    pub fn len(&self) -> usize; pub fn is_empty(&self) -> bool;
}
```
A refusal leaves the card open (a stranger's answer must not close the owner's card). Late answers:
`Refused(Late)` and the card stays until `expire`.

Tests (cards.rs): accepted owner answer (and card removed); stranger → NotOwner and still open;
unknown card id → UnknownCard; a value not offered → NotOffered; two values for max 1 → NotOffered;
duplicates → NotOffered; after deadline → Late; a text message → NotACardAnswer; answer before
`sent` (message_id unknown) → UnknownCard; `expire` returns only past-deadline cards.
Build answer `Message`s with `MessageKind::Answer` and body `{"card": "<uuid>", "values": [...], "fallback": "x"}`.

- [ ] Steps: tests → implement → `cargo test -p push-sdk cards` → clippy → commit
  `feat(sdk): CardBook checks answers to an agent's cards`.

---

### Task 4: crate `push-agent-key` — Windows CNG signer and portable software signer

**Cargo.toml:** name `push-agent-key`, workspace package fields, `publish = false`. Deps:
`push-sdk` (workspace), `ring` (workspace), `base64`? (only if needed), `thiserror`.
`[target.'cfg(windows)'.dependencies] windows-sys = { version = "0.61", features = ["Win32_Foundation", "Win32_Security_Cryptography"] }`.
dev-deps: `push-auth-core = { workspace = true, features = ["testing"] }`, `serde_json`.
Lints (not `workspace = true`, because NCrypt needs unsafe):
```toml
[lints.rust]
# Only src/cng.rs uses unsafe (NCrypt FFI), each block with a SAFETY note.
unsafe_code = "deny"
unsafe_op_in_unsafe_fn = "deny"
[lints.clippy]
all = { level = "warn", priority = -1 }
undocumented_unsafe_blocks = "warn"
```
`src/cng.rs` starts with `#![allow(unsafe_code)]`. Add the crate to root `members` and
`[workspace.dependencies] push-agent-key = { path = "crates/push-agent-key" }`.

**API (`src/lib.rs`):**
```rust
pub use push_sdk::{AgentClaims, AgentPublicKey, AgentSigner, SignerError};
/// A signer that can also tell its public key and where the key lives.
pub trait AgentKey: AgentSigner {
    fn public_key(&self) -> AgentPublicKey;
    /// Where the private key is, for people: "Microsoft Platform Crypto Provider (TPM)", "software file …".
    fn storage(&self) -> String;
}
pub mod software;  // SoftwareKey
#[cfg(windows)] pub mod cng; // CngKey
```
`software::SoftwareKey` — **for tests and development only** (doc says so loudly): PKCS#8 P-256 via
`ring::signature::EcdsaKeyPair` with `ECDSA_P256_SHA256_FIXED_SIGNING` (64-byte r||s).
`SoftwareKey::generate() -> Result<Self, SignerError>`, `from_pkcs8(&[u8])`,
`load_or_create(path: &Path)` (writes the PKCS#8 file with the default permissions of the
user's folder; creates parent dirs). Public key from `key_pair.public_key()` (65-byte uncompressed `04||x||y`).
`sign` uses `push_sdk::assemble_assertion`.

`cng::CngKey` (Windows): `open_or_create(name: &str) -> Result<Self, SignerError>`: for each provider
in `["Microsoft Platform Crypto Provider", "Microsoft Software Key Storage Provider"]` try
`NCryptOpenStorageProvider` then `NCryptOpenKey(name)`; if found use it. Otherwise create in the
first provider that succeeds: `NCryptCreatePersistedKey(BCRYPT_ECDSA_P256_ALGORITHM, name)`,
set `NCRYPT_EXPORT_POLICY_PROPERTY` to 0 (not exportable), `NCryptFinalizeKey`. `open(name)` (no create),
`delete(name)` (open + `NCryptDeleteKey`), `provider()`.
Public key: `NCryptExportKey(BCRYPT_ECCPUBLIC_BLOB)` (public export works for non-exportable keys) →
`BCRYPT_ECCKEY_BLOB { magic, cbKey=32 } || X || Y`. Sign: SHA-256 the signing input with
`ring::digest::SHA256`, `NCryptSignHash(key, null padding, hash, …)` → 64-byte r||s (P1363 already).
Errors: `SignerError { code: format!("0x{:08X}", status as u32) plus a name for common
NTE_* codes, message: "<operation> failed" }` — never key bytes. Wrap handles in RAII
(`NCryptFreeObject` on drop). `CngKey` must be `Send + Sync` (NCRYPT handles are usable across
threads; implement `unsafe impl Send/Sync` with a SAFETY note).

**Tests:**
- `software`: `the_signature_passes_the_servers_verifier`: generate key, `AgentClaims::new("https://push.test", now)`,
  `sign`, then `push_auth_core::agent_key::verify_assertion(&token, "https://push.test", now)` is
  `Ok(thumbprint)` and equals `public_key().thumbprint()`. Also a wrong audience → Err, and two
  signatures of fresh claims have different jtis. `load_or_create` twice → same public key.
- `cng` (`#[cfg(windows)]`, runs locally on Windows only): create a key named
  `push-agent-key-test-<uuid>` **in the software KSP** (expose `create_in(provider, name)` for
  tests, or an env-free internal fn), sign, verify with the server verifier, check export of the
  private key fails (`NCryptExportKey` with `BCRYPT_ECCPRIVATE_BLOB` returns an error), then delete.
- [ ] Steps: tests → implement → `cargo test -p push-agent-key` (on this Windows host it runs the CNG
  test too) → `cargo clippy -p push-agent-key --all-targets -- -D warnings` → `cargo deny check`
  if available (`cargo deny --version`; otherwise skip and note) → commit
  `feat(agent-key): CNG and software signers for agent-key sign-in`.

---

### Task 5: crate `push-bridge` foundations — masking, commands, folders, config, state, backend trait, backoff

**Cargo.toml:** package `push-bridge`, `publish = false`, `[[bin]] name = "push-agent-bridge" path = "src/main.rs"`
(main.rs may be a stub printing usage in this task), `[lints] workspace = true`.
Deps: push-sdk, push-domain, push-agent-key, tokio (features macros, rt-multi-thread, sync, time,
process, io-util, fs, signal), async-trait, serde, serde_json, regex = "1", thiserror, tracing,
tracing-subscriber, uuid. dev-deps: tokio with `test-util`, tempfile? (NOT in workspace — create
temp dirs under `std::env::temp_dir()` with a uuid instead, to avoid new deps).
Add `regex = "1"` to `[workspace.dependencies]` and use `regex.workspace = true`.

**`mask.rs`:**
```rust
pub const MASK: &str = "«masked»";
/// Masks every secret pattern (see below) in `text`.
pub fn mask(text: &str) -> String;
/// At most `max` characters; when cut, ends with "…" (the result has `max` chars).
pub fn cap(text: &str, max: usize) -> String;
/// `cap(mask(text), max)`.
pub fn masked(text: &str, max: usize) -> String;
/// One line: line breaks and tabs become spaces, runs of spaces collapse; then masked and capped.
pub fn masked_line(text: &str, max: usize) -> String;
```
Patterns (compile once with `std::sync::LazyLock` — stable since 1.80, OK), applied in this order:
1. PEM private-key blocks: `-----BEGIN [A-Z0-9 ]*KEY-----` … `-----END [A-Z0-9 ]*KEY-----` (dot matches newline; an unterminated BEGIN masks to end of text).
2. Bearer tokens: `(?i)\bbearer\s+[A-Za-z0-9\-._~+/]+=*` → `Bearer «masked»`.
3. JWTs: `\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*` (also JWS with empty signature).
4. Connection-string secrets: `(?i)\b(AccountKey|SharedAccessKey|SharedAccessSignature)=[^;\s"']+` → keeps `Name=`.
5. `name=value` / `name: value` / `"name": "value"` / `'name'='value'` where the name contains
   `key|secret|token|password|pwd` (case-insensitive; e.g. `api_key`, `clientSecret`, `PGPASSWORD`):
   keep the name and delimiter, mask the value (a quoted value up to its closing quote, or an unquoted
   run up to whitespace, `;`, `,`, `&`, `}`, `)`). Also command-line flags `--password X`, `--token X`, `-p`? (only the long flags whose name contains the words).
6. Hex runs ≥ 40 chars: `\b[0-9a-fA-F]{40,}\b`; base64/base64url runs ≥ 40 chars: `[A-Za-z0-9+/_-]{40,}={0,2}`
   (must contain at least one digit or `+/_-` mix? — no: the spec says runs of 40+, mask them; but
   do NOT mask paths: a run is only the token chars, `/` breaks Windows-irrelevant… to avoid masking
   long Unix paths like `/usr/local/lib/…`, exclude `/` from a run unless the run also contains a `+`
   or ends with `=`; document this choice in a comment and test it).
Must-not-mask tests: `src/main.rs`, `C:\Users\me\project\file.txt`, `/usr/local/share/doc/package/README.md`,
`cargo test --workspace`, `the key is in the drawer`, `token count: 12`?? (decide: `token count: 12` →
name `token count` has a space → not a `name: value` pair because names are identifiers without
spaces: masks only identifier names `[A-Za-z0-9_.-]*(key|secret|token|password|pwd)[A-Za-z0-9_.-]*`);
`monkey=banana` contains "key" → masked (accepted false positive; test documents it), a UUID
`123e4567-e89b-12d3-a456-426614174000` (36 chars) not masked, a 39-char hex not masked, git short
sha not masked. Masking tests for every pattern (one test each + the must-not-mask list).

**`commands.rs`:**
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    New, Stop, Status, Help,
    Cd(String),               // raw argument (may be empty → usage)
    Compact,
    Model(Option<String>),
    Effort(String),           // validated later: low|medium|high|xhigh|max|auto
    Mode(String),             // default|acceptEdits|plan|bypass
    Goal(String),             // may be empty → clear goal
    Sessions, Resume(String), Fork,
    Tasks, TaskStop(String),
    Btw(String),
    ScheduleList, ScheduleAdd { when: ScheduleWhen, prompt: String }, ScheduleRemove(String),
    Skills,
    /// Any other `/…`: sent to the backend as a prompt.
    Passthrough(String),
    /// A known command with bad arguments: the usage line to reply with.
    Usage(&'static str),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleWhen { Every(String), At(String), Cron(String) }
/// `None` when `text` is not a command (does not start with '/').
pub fn parse(text: &str) -> Option<Command>;
pub const HELP: &str = "…lists every command with one line each…";
```
Parsing: trim; command word is case-sensitive lower-case as in the table; `/schedule add every 1h run tests`
→ `Every("1h")`, prompt "run tests"; `/schedule add cron "0 9 * * 1-5" standup` → cron spec may be quoted
(double quotes) because it has spaces; unquoted cron takes 5 space-separated fields. `/task stop <id>`.
`/mode bypass` → `Mode("bypass")`. `/effort` with invalid value → `Usage`. Tests for every row of the
request's Commands table, the passthrough (`/review this` → Passthrough("/review this")), `//x`?
(→ Passthrough), plain text → None.

**`folders.rs`:**
```rust
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FolderError { #[error("no such folder")] NotFound, #[error("not a folder")] NotADirectory, #[error("outside the allowed roots")] OutsideRoots }
/// The real path of `requested` (symlinks and `..` resolved by `std::fs::canonicalize`) when it is a
/// directory equal to or inside one of `roots` (each canonicalized too; roots that do not exist are
/// skipped). Relative paths resolve against `current`. On Windows the `\\?\` prefix of a local path
/// is removed from the result (keep `\\?\UNC\` → `\\` conversion too).
pub fn resolve(requested: &str, current: &Path, roots: &[PathBuf]) -> Result<PathBuf, FolderError>;
```
Containment uses `Path::starts_with` on canonical paths (component-wise: `C:\src2` is not inside `C:\src`).
Tests: inside OK, the root itself OK, `..` escape refused, sibling with common prefix refused, a file
refused, missing refused, and a **symlink escape**: inside root create a dir symlink pointing outside →
refused. Creating symlinks on Windows needs privilege/developer mode: use
`std::os::windows::fs::symlink_dir` and, if it fails with a permission error, fall back to a
junction via `cmd /C mklink /J` (junctions need no privilege); on Unix `std::os::unix::fs::symlink`.
If neither works, the test prints a note and returns (do not fail on hosts that forbid both).

**`config.rs`** (serde, `#[serde(deny_unknown_fields)]`, file `config.json`):
```rust
pub struct Config {
    pub api_url: String,                    // https://push.example.com (auth-svc public URL)
    pub socket_url: Option<String>,         // default: api_url with https→wss, http→ws, + "/v1/socket"
    pub host: Option<String>,               // Host header override (dev clusters)
    pub audience: Option<String>,           // default: api_url without trailing '/'
    pub allow_insecure_transport: bool,     // default false
    pub key: KeyConfig,                     // default { name: "push-messenger-coda-bridge", storage: Windows }
    pub device_name: Option<String>,        // default: COMPUTERNAME / HOSTNAME / "push-agent-bridge"
    pub roots: Vec<PathBuf>,                // at least one
    pub default_folder: PathBuf,            // must be inside roots (checked at start with folders::resolve)
    pub default_permission_mode: PermissionMode, // default|acceptEdits|plan (bypass refused here)
    pub backend: BackendKind,               // "coda" (only value)
    pub coda: Option<CodaConfig>,           // required when backend == coda
}
pub struct KeyConfig { pub name: String, pub storage: KeyStorage }
pub enum KeyStorage { Windows /* "windows": CNG, TPM first */, SoftwareFile /* "software-file": <dir>/agent-key.p8, dev/test only */ }
pub struct CodaConfig { pub exe: PathBuf, #[serde(default)] pub args: Vec<String> }
pub fn default_dir() -> PathBuf;  // %LOCALAPPDATA%\push-messenger\coda-bridge; non-Windows: $XDG_STATE_HOME or ~/.local/state + push-messenger/coda-bridge
impl Config { pub fn load(path: &Path) -> Result<Self, ConfigError>; pub fn validate(&self) -> Result<(), ConfigError>; pub fn socket_url(&self) -> String; pub fn audience(&self) -> String; }
```
`PermissionMode` lives in backend.rs (below) with serde names `default`, `acceptEdits`, `plan`,
`bypassPermissions`. Tests: a full example parses; minimal parses with defaults; unknown field
refused; `backend: "cortex"` refused with a clear message; socket url derivation; bypass default refused.

**`state.rs`:** `state.json` next to config (atomic save: write `state.json.tmp`, then rename).
```rust
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub device_id: Option<String>,          // the SDK's push.device value (not secret)
    pub registration_id: Option<String>,    // the SDK's push.registration value (not secret)
    pub enrolled_user_id: Option<String>,
    pub folder: Option<PathBuf>,            // current working folder
    pub sessions: BTreeMap<String, String>, // folder (display string of the real path) → session id
    pub agent_message_cursor: Option<u64>,
    pub engine_instance: Option<String>,
}
pub struct StateFile { path: PathBuf, state: Mutex<State> }  // load(path), get() -> State, update(|s| …) saves
/// A push_sdk::SecretStore that keeps tokens in memory only and persists the device and registration
/// ids (not secret) in the state file — so a restart signs the same device in again.
pub struct BridgeSecretStore { file: Arc<StateFile>, memory: Mutex<HashMap<String, String>> }
```
Check `push_sdk::SecretStore`'s exact trait methods and the stored value formats of `push.device`/
`push.registration` in `crates/push-sdk/src/{secrets.rs,credentials.rs}` and persist those values
verbatim. Tests: round trip, atomic save leaves no tmp, secret store persists device but never the
session key (`push.session` not in the file after `set`).

**`backend.rs`** — the interface between core and backends:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Capability { Steer, PlanApproval, Tasks, Sessions, Fork, Compact, Models, Effort, PermissionModes, Goal, AskAside, Schedules, Skills, AgentMessages }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode { #[serde(rename="default")] Default, #[serde(rename="acceptEdits")] AcceptEdits, #[serde(rename="plan")] Plan, #[serde(rename="bypassPermissions")] BypassPermissions }
impl PermissionMode { pub fn as_str(self) -> &'static str; pub fn parse(&str) -> Option<Self>; }
pub struct StartOptions { pub folder: PathBuf, pub session: Option<String>, pub permission_mode: PermissionMode }
pub struct Started { pub session_id: String, pub engine_instance: Option<String>, pub version: Option<String> }
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind { Busy, SessionNotFound, NotRunning, Refused, Other }
#[derive(Debug, Clone, thiserror::Error)]
#[error("{method} failed ({code}): {message}")]
pub struct BackendError { pub method: String, pub code: String /* "-32603" or "exited" */, pub message: String, pub kind: ErrorKind }
pub enum SteerOutcome { Accepted, NoActiveTurn, TurnEnding, Rejected(String) }
pub enum TurnSource { Prompt, Engine }
pub enum RequestKind {
    Permission { tool: String, preview: String },
    Question { question: String, options: Vec<String>, multi_select: bool, allow_free_text: bool },
    Plan { plan: String },
}
pub enum Answer { Permission(bool), Question(String), Plan(bool), Decline }
/// The way back to the backend for one request. Dropping it without answering declines.
pub struct Responder(tokio::sync::oneshot::Sender<Answer>);
impl Responder { pub fn new() -> (Self, oneshot::Receiver<Answer>); pub fn answer(self, answer: Answer); }
pub struct BackendRequest { pub kind: RequestKind, pub responder: Responder }
pub enum TaskStatus { Completed, Failed, Stopped }
pub enum BackendEvent {
    TurnStarted { source: TurnSource },
    ThinkingDelta(String),
    TextDelta(String),
    ToolStarted { id: String, tool: String, target: String, task: Option<String> },   // task: the subagent's task description when the tool runs in a subagent
    ToolFinished { id: String, failed: bool },
    Request(BackendRequest),
    AgentMessage { cursor: u64, label: String, text: String },
    TaskStarted { id: String, description: String, background: bool },
    TaskCompleted { id: String, description: String, status: TaskStatus, background: bool, report: Option<String> },
    Error(String),
    TurnEnded { interrupted: bool },
    /// The backend process stopped or its stream broke; open requests are already declined.
    Exited { reason: String },
}
pub enum BackendCommand {
    Compact, Models, SetModel(String), SetEffort(String), SetPermissionMode(PermissionMode), SetGoal(Option<String>),
    ListSessions, Fork, ListTasks, StopTask(String), AskAside(String),
    ScheduleList, ScheduleAdd { when: ScheduleWhen, prompt: String }, ScheduleRemove(String), Skills,
}
pub struct CommandReply { pub text: String, pub session: Option<String> /* a new session id (fork) */ }
pub struct BackendStatus { pub model: Option<String>, pub version: Option<String>, pub running_tasks: Option<usize> }
pub struct PendingMessages { pub messages: Vec<(u64, String, String)> /* cursor, label, text */, pub lost: bool }
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// "Coda": used in replies such as "(Coda stopped; restarting)".
    fn name(&self) -> &str;
    fn capabilities(&self) -> BTreeSet<Capability>;
    /// Starts the agent in `options.folder` (restarting its process when the folder changed or it is
    /// not running) and opens `options.session`, or a new session when `None`.
    /// `ErrorKind::SessionNotFound` when the session is unknown in that folder.
    async fn start(&self, options: StartOptions) -> Result<Started, BackendError>;
    async fn stop(&self);
    /// Runs one prompt; resolves when its turn ends. `ErrorKind::Busy` when a turn is running.
    async fn prompt(&self, text: &str) -> Result<(), BackendError>;
    async fn steer(&self, text: &str) -> Result<SteerOutcome, BackendError>;
    async fn interrupt(&self) -> Result<(), BackendError>;
    async fn command(&self, command: BackendCommand) -> Result<CommandReply, BackendError>;
    async fn status(&self) -> Result<BackendStatus, BackendError>;
    /// Background messages after `cursor` (None: from the start) of `engine_instance`.
    async fn pending_messages(&self, after: Option<u64>, engine_instance: Option<&str>) -> Result<PendingMessages, BackendError>;
}
```
Backends are constructed with an `mpsc::UnboundedSender<BackendEvent>` and send every event through it.

**`backoff.rs`:** `Backoff { base: 1 s, cap: 60 s }` with `next_delay()` (1, 2, 4, …, 60, 60) and
`reset()`; the core resets after the backend ran ≥ 60 s. Tests for the sequence and reset.

- [ ] Steps: one module at a time, tests first; `cargo test -p push-bridge`; clippy; commit after each
  module or two (`feat(bridge): …`).

---

### Task 6: rendering and cards

**`render.rs` — `TurnRenderer`** (pure; no I/O; times passed in as `tokio::time::Instant`):
```rust
pub const EDIT_INTERVAL: Duration = Duration::from_secs(2);
pub const TEXT_PAGE: usize = 4_000;   // chars of text per reply
pub const THINKING_TAIL: usize = 4_000;
pub const MAX_STEPS: usize = 50;
pub const STEP_LABEL: usize = 80;
pub struct ReplyUpdate { pub page: usize /* 0-based reply index in this turn */, pub body: String /* agent.reply@1 RICH JSON */, pub new: bool /* send, else edit */ }
impl TurnRenderer {
    pub fn new(now: Instant, title: Option<String>) -> Self;
    pub fn thinking(&mut self, delta: &str);
    pub fn text(&mut self, delta: &str);
    pub fn tool_started(&mut self, id: &str, tool: &str, target: &str, task: Option<&str>, now: Instant);
    pub fn tool_finished(&mut self, id: &str, failed: bool, now: Instant);
    /// A background task's completion as a step in this reply.
    pub fn task_step(&mut self, label: &str, failed: bool);
    pub fn note(&mut self, text: &str);   // appended to the text on its own line, e.g. "(Coda stopped; restarting)"
    /// What to send now: the first reply at once, then edits at most every EDIT_INTERVAL (only when
    /// something changed), plus a new reply each time the text passes another 4,000 chars (the
    /// previous page gets its final edit in the same call).
    pub fn poll(&mut self, now: Instant) -> Vec<ReplyUpdate>;
    /// The final state of every page that changed since its last update (interrupted: text ends "(stopped)").
    pub fn finish(&mut self, interrupted: bool) -> Vec<ReplyUpdate>;
    pub fn current_label(&self) -> String;  // activity label: running tool "<tool> <target>" or "thinking"; masked_line ≤ 60
}
```
Body format: `{"tpl":"agent.reply@1","params":{…},"fallback":"…"}` built with serde_json.
Params: `title` (optional, masked, ≤120 chars), `text` (masked page text; when empty use "…"),
`thinking` (masked tail of the last 4,000 chars of thinking; omitted when empty; only on page 0 —
later pages carry only their text and steps added after the split),
`steps` (items `{label, status, detail?}`; status `running|done|failed`; detail = duration like
`1.2 s`/`2 min 5 s` once finished; label = `masked_line(format!("{tool} {target}"), 80)`, prefixed by
`masked_line(task, …)` + `: ` for subagent tools so it reads `<task>: <tool> <target>` within 80 chars;
at most 50: beyond 50 the oldest *finished* steps are dropped, keeping the count at 50).
`fallback`: the masked page text capped to the fallback limit (check the limit in push-domain body.rs;
non-empty, use "…" when empty).
Masking: the whole accumulated text is masked first, then split into pages by chars (prefer the last
newline or space within the final 200 chars of a page boundary); a page that was finalized is never edited again.
Every body produced in tests must pass `RichBody::parse(body)?.check(&agent.reply@1 definition)`
(get the definition from `push_domain::templates::builtins()`).
Tests: first poll sends page 0 at once; deltas within 2 s → no update; after 2 s → one edit; no change →
no edit; thinking keeps the last 4,000 chars; 60 tools → 50 steps; step running→done with duration
detail; subagent label; text of 9,000 chars → 3 pages, earlier pages final; a secret in text and in a
tool target is masked; `finish(true)` ends text with "(stopped)"; all bodies valid against the template.

**`cards.rs`** — building cards and mapping answers:
```rust
pub const CARD_TIMEOUT: Duration = Duration::from_secs(600);
pub enum CardKind { Permission, Question { options: Vec<String>, multi: bool, free_text: bool }, Plan, Bypass }
pub struct CardPlan {
    /// RICH body of the card, or None when the request is shown as text only (> 10 options).
    pub card: Option<String>,
    /// agent.reply@1 bodies to post after the card (the full plan, the text-only question).
    pub follow_ups: Vec<String>,
    /// Offered values and max values for CardBook.
    pub offered: Vec<String>, pub max_values: usize,
    pub kind: CardKind,
    /// Activity label while open: "approval needed", "question", "plan approval".
    pub waiting_label: &'static str,
}
pub fn permission_card(backend: &str, tool: &str, preview: &str, folder: &Path) -> CardPlan;
    // agent.approval@1: title masked_line("Allow {tool}?", 120); details "{backend} wants to use {tool} in {folder}."; preview masked(preview, 4000) (omit when empty)
pub fn question_card(question: &str, options: &[String], multi: bool, free_text: bool) -> CardPlan;
    // ≤ 10 options: agent.question@1 (single) or agent.choose@1 (multi); option ids "o1".."o10", labels masked_line(label, …limit from body.rs)
    // > 10 options: card None; follow_ups = [agent.reply@1 with the question and a numbered option list, "Reply with the option's number or label." / for multi "…numbers or labels, separated by commas."]
pub fn plan_card(plan: &str) -> CardPlan;
    // agent.plan-approval@1 title "Approve the plan?"; plan masked; if > 4,000 chars: cut to fit with "\n\n(The plan is cut here; the full text follows.)" inside the 4,000, and follow_ups = the full masked plan as agent.reply@1 pages of 4,000 chars titled "Plan (1/N)"
pub fn bypass_card(backend: &str) -> CardPlan;
    // agent.approval@1 "Turn off approvals?", details "{backend} will run every tool without asking until /new. Approve to switch to bypassPermissions."
/// From the owner's card answer values to the backend answer.
pub fn answer_from_values(kind: &CardKind, values: &[String]) -> Answer;
    // Permission/Plan/Bypass: values == ["approve"] → true else false. Question: ids → labels in option order joined with ", ".
pub enum TypedOutcome { Answer(Answer), Retry(&'static str) /* reply text, card stays open */ }
/// The owner typed text while this card is the oldest open one.
pub fn answer_from_text(kind: &CardKind, text: &str) -> TypedOutcome;
    // Permission/Bypass: trim, case-insensitive "yes"/"y" → true; anything else → false.
    // Plan: always Plan(false) (anything but the button rejects).
    // Question: exact option label (case-insensitive, trimmed) or 1-based number → that label (multi: comma-separated list → labels in option order, ", "-joined);
    //   otherwise free_text ? Question(text) : Retry("Pick one of the options").  Empty text → Retry.
/// What the backend gets when the card times out: Permission(false) / Plan(false) / Decline (question) / Bypass → no change.
pub fn timeout_answer(kind: &CardKind) -> Option<Answer>;
```
Tests: every builder's body validates against its built-in definition (`agent.approval@1`,
`agent.question@1`, `agent.choose@1`, `agent.plan-approval@1`, `agent.reply@1`); offered values match
`RichBody::offered`; 11 options → text only; multi answer order is option order not click order;
typed text rules; plan > 4,000 cut with note and full text follow-ups; preview masked.

- [ ] Steps: tests → implement → test/clippy → commit `feat(bridge): reply rendering and cards`.

---

### Task 7: Coda backend

Read the Coda docs listed at the top (serve-protocol.md §Lifecycle, requests, notifications,
server-initiated requests, fail-closed; catalog.md; catalog.json for exact param/result names of
`initialize`, `session/steer`, `session/pendingMessages`, `session/listTasks`, `session/models`,
`session/schedule*`, `skills/list`, `event/activity`, `event/turnEnded`, `event/agentMessage`, task events).

**`coda/framing.rs`:** `FrameReader<R: AsyncRead>` (`next() -> io::Result<Option<serde_json::Value>>`;
parses `Content-Length: N\r\n` headers (case-insensitive, ignore other headers such as
Content-Type), blank line, N bytes; refuses N > 64 MiB; EOF between frames → `None`; EOF inside a
frame → error) and `write_frame(w, &Value)`. Tests: one frame, two frames in one read, a frame split
across many 1-byte reads (use `tokio_test`? no — implement a tiny AsyncRead that yields chunks, or
use `tokio::io::duplex` writing byte by byte), extra header, bad header → error, truncated → error.

**`coda/rpc.rs`:** JSON-RPC 2.0 peer over a reader/writer pair:
`RpcPeer::new(reader, writer) -> (RpcPeer, mpsc::UnboundedReceiver<Incoming>)` where
`Incoming::Notification { method, params }` / `Incoming::Request { id: Value, method, params }` /
`Incoming::Closed { reason }`. `peer.call(method, params, timeout: Option<Duration>) -> Result<Value, RpcError>`
(`RpcError::Remote { code: i64, message }`, `Closed`, `Timeout`), `peer.respond(id, result)`,
`peer.respond_error(id, code, message)`. Responses are matched by numeric id; on close every pending
call fails with `Closed`. Tests with `tokio::io::duplex` and a scripted peer: call/response, error,
interleaved notifications, a request from the peer, close fails pending calls, timeout.

**`coda/backend.rs` — `CodaBackend: Backend`:**
- `CodaBackend::new(config: CodaConfig, events: UnboundedSender<BackendEvent>)` plus
  `CodaBackend::with_launcher(launcher: Arc<dyn Launcher>, events)` for tests, where
  ```rust
  pub struct Spawned { pub reader: Box<dyn AsyncRead + Send + Unpin>, pub writer: Box<dyn AsyncWrite + Send + Unpin>, pub exited: oneshot::Receiver<String> /* exit status text */, pub kill: Box<dyn FnOnce() + Send> }
  #[async_trait] pub trait Launcher: Send + Sync { async fn spawn(&self, folder: &Path, mode: PermissionMode) -> io::Result<Spawned>; }
  ```
  The real `ProcessLauncher` runs `tokio::process::Command::new(exe).arg("serve").args(config.args)
  .arg("--cwd").arg(folder).arg("--permission-mode").arg(mode)` with stdin/stdout piped, stderr null
  (or piped and drained/discarded — never logged), `kill_on_drop(true)`, **`env_remove("CODA_SERVE_API_KEY")`**;
  arguments always as a list, never through a shell. Check the exact `--permission-mode` spellings in
  serve-protocol.md (`default`, `accept-edits`/`acceptEdits`, `plan`, `bypass`/`yolo`): map
  `PermissionMode` to the CLI spelling the docs give and to the `session/setPermissionMode` spelling
  catalog.json gives — they may differ; test the mapping.
- `start`: if no process or the folder differs → (kill old) spawn; then `initialize {protocolVersion:"1",
  sessionId?, clientCapabilities:{stateEvents:true}, clientInfo:"push-agent-bridge"}` → `Started{session_id,
  engine_instance: engineInstanceId, version: serverInfo}`. `-32002` → `ErrorKind::SessionNotFound`.
  If the mode differs from the spawn's, call `session/setPermissionMode`.
- Notifications → `BackendEvent`s: `event/activity` (first per turnId → `TurnStarted`; `source` from
  the activity/turn params: a turn whose start was not caused by our pending prompt → `Engine`; track
  "prompt in flight" to decide), `event/turnEnded` → `TurnEnded{interrupted}` (read the field names in
  catalog.json; fall back to `event/turnComplete` only if state events never arrived), `event/thinking`,
  `event/assistantText`, `event/toolCall` → `ToolStarted` (id = callId or a generated one; target =
  the first present of `path`, `file_path`, `command`, `cmd`, `query`, `pattern`, `url`, `description`
  in inputJson (parse; on failure use the raw string), single line; `task` = description of the task named
  by `sourceId` `subagent:<taskId>` from `event/taskStarted`), `event/toolResult` → `ToolFinished`
  (match by callId, else the oldest running tool of that name; failed = isError or status Failed/Cancelled),
  `event/error` → `Error`, `event/agentMessage` → `AgentMessage`, `event/taskStarted/taskCompleted` →
  Task events (`background` = `mode == "background"`; only tasks without `ownerTaskId` count as the main
  agent's), unknown → ignored.
- Requests from Coda: `request/permission|question|planApproval` → `BackendEvent::Request` with a
  `Responder`; a task awaits the receiver: `Answer::Permission(b)` → `{allow:b}`, `Question(s)` →
  `{answer:s}` (an empty string must never be sent: send -32800 instead), `Plan(b)` → `{approve:b}`,
  `Decline` or dropped responder → JSON-RPC error `-32800` "declined". Unknown request methods →
  `-32601`.
- `prompt` → `session/prompt {text}`; an error whose message contains "busy" (case-insensitive) or a
  -32603 with busy → `ErrorKind::Busy`. No timeout.
- `steer` → `session/steer {text}` → `{ok, rejectedReason}` mapping.
- `interrupt` → `session/interrupt`. `command`: Compact → `session/compact {}`; Models → `session/models {}`
  (reply lists ids, marking the current one if reported); SetModel → `session/setModel {model}`;
  SetEffort → `session/setEffort {effort}`; SetPermissionMode → `session/setPermissionMode {mode}`;
  SetGoal → `session/setGoal {goal}`; ListSessions → `session/listSessions {limit: 20}` (one line per
  session: id, created, message count, preview single-line capped 80, "(current)"); Fork →
  `session/fork {}` → `CommandReply.session` = new id; ListTasks → `session/listTasks {statusFilter:"active"}`;
  StopTask → `task/stop {taskId}`; AskAside → `session/askAside {question}` (answer or reason);
  ScheduleList/Add/Remove → `session/scheduleList` / `scheduleCreate {prompt, every|at|cron}` /
  `scheduleDelete {id}`; every schedule reply ends with "Schedules live in Coda's memory and are lost
  when Coda restarts."; Skills → `skills/list`. Format replies as short plain text (the core masks).
- `status` → `session/models` (current model; tolerate absence) + `session/listTasks {statusFilter:"active"}` count + version from initialize.
- `pending_messages` → `session/pendingMessages {afterCursor, limit: 100, engineInstanceId?}`; follow
  `truncated/nextCursor`; `gap` → `lost: true`.
- Process exit or stream close → decline every open request (their responders' tasks fail with the
  closed peer — make sure nothing hangs) and send `BackendEvent::Exited{reason}` exactly once per process.
  The core restarts (Task 8). `stop` sends `shutdown` (1 s timeout) then kills.
- `capabilities`: all of Steer, PlanApproval, Tasks, Sessions, Fork, Compact, Models, Effort,
  PermissionModes, Goal, AskAside, Schedules, Skills, AgentMessages.

Tests (in-process, `Launcher` returning `tokio::io::duplex` ends driven by a scripted fake Coda
written in the test module — a helper `FakeCoda` that reads frames, answers `initialize`, and lets
the test push notifications/requests): initialize with/without session and stateEvents capability;
`-32002` → SessionNotFound; prompt → turn events mapped in order (activity → TurnStarted(Prompt);
thinking/text/toolCall/toolResult; turnEnded); engine-started turn (activity without a prompt) →
TurnStarted(Engine); busy error → Busy; steer outcomes incl. noActiveTurn/turnEnding; permission
request answered allow; question answered; Decline → -32800 error frame; dropped responder → -32800;
empty question answer → -32800; Coda exiting (drop the fake's ends) → Exited and pending prompt fails;
start after exit respawns; folder change respawns with the new `--cwd` (launcher records args);
`CODA_SERVE_API_KEY` removed (unit-test the `Command` builder function that returns the args and
env removals); pendingMessages paging; subagent tool label via taskStarted.

- [ ] Steps: framing → rpc → backend, tests first each; commit after each
  (`feat(bridge): Content-Length framing and JSON-RPC peer`, `feat(bridge): Coda backend`).

---

### Task 8: the core, messenger abstraction, TestBackend, integration tests

**`messenger.rs`:**
```rust
#[async_trait::async_trait]
pub trait Messenger: Send + Sync + 'static {
    fn me(&self) -> UserId;
    /// The agent's owner, from its own profile (owner_user_id).
    async fn owner(&self) -> Result<UserId, SdkError>;
    async fn direct(&self, peer: UserId) -> Result<ConversationId, SdkError>;
    fn send_rich(&self, conversation: ConversationId, body: &str) -> Result<ClientMsgId, SdkError>;
    fn edit_rich(&self, conversation: ConversationId, id: ClientMsgId, body: &str) -> Result<(), SdkError>;
    fn send_text(&self, conversation: ConversationId, text: &str) -> Result<ClientMsgId, SdkError>;
    async fn activity(&self, conversation: ConversationId, state: ActivityState, label: Option<&str>) -> Result<(), SdkError>;
}
pub struct SdkMessenger { client: push_sdk::Client, me: UserId }   // wraps Client; events come from push_sdk::Events
pub struct TestMessenger { … }  // records every call (sent/edited bodies, texts, activities), lets the
// test inject push_sdk::Event values into the core's event channel, and simulates MessageSent (assigns
// message ids/seq) and SendFailed for chosen bodies (e.g. `fail_next_rich()`).
```
**`test_backend.rs` — `TestBackend: Backend`** (public, used by integration tests): records calls
(`calls() -> Vec<String>` like `"start folder=… session=None"`, `"prompt hi"`, `"steer x"`), lets the
test emit `BackendEvent`s, configure the next results (`prompt` busy, steer outcome, start
SessionNotFound once), and capture `Responder` answers (`Request` events created by
`test_backend.request(kind) -> oneshot::Receiver<Answer>`). Capabilities configurable (prove the core
hides commands a backend lacks: `/fork` on a backend without Fork → "Not supported by <name>.").

**`core.rs` — `Bridge`:**
```rust
pub struct BridgeOptions { pub roots: Vec<PathBuf>, pub default_folder: PathBuf, pub default_mode: PermissionMode }
pub struct Bridge<M: Messenger> { … }
impl<M: Messenger> Bridge<M> {
    pub fn new(messenger: Arc<M>, backend: Arc<dyn Backend>, backend_events: UnboundedReceiver<BackendEvent>,
               messenger_events: mpsc::Receiver<push_sdk::Event>, state: Arc<StateFile>, options: BridgeOptions) -> Self;
    /// Runs until `shutdown` resolves or the messenger event stream ends; returns why it stopped.
    pub async fn run(self, shutdown: impl Future<Output = ()> + Send) -> Result<(), BridgeError>;
}
```
Behaviour (each line is a test in `core` unit tests or `tests/bridge.rs`; use `#[tokio::test(start_paused = true)]` for timing):
- **Start:** owner = `messenger.owner()`; conversation = `direct(owner)`; folder = state.folder (if still
  valid via `folders::resolve`) else default; `backend.start{folder, session: state.sessions[folder], mode: default}`;
  SessionNotFound → start again with `None` (ids never move between folders); store session id; record engine
  instance; if the stored engine instance equals the new one, fetch `pending_messages(cursor)`; else reset cursor.
- **Owner gate:** `MessageAdded` from anyone other than me/owner (text or rich) → `send_text(their conversation,
  "This agent is private.")` at most once per sender per hour; their text never reaches the backend. Own
  messages (sender == me) ignored. Answers (`MessageKind::Answer`) go to the CardBook (strangers' answers refused silently).
  Deduplicate by (conversation, seq) so a re-delivered message is handled once.
- **Owner text:** command → run it. Else oldest open card → `cards::answer_from_text` (Retry → reply text,
  card stays). Else if a turn is running (core's own flag from TurnStarted/TurnEnded) and the backend has
  `Steer` → `steer`; `NoActiveTurn`/`TurnEnding` → queue text, send as prompt when the turn ends. Else `prompt`
  (spawned; `Busy` → steer; other errors → reply "session/prompt failed (<code>): <message>" masked).
  Without `Steer` capability while busy → queue for after the turn.
- **Turns:** `TurnStarted` creates a `TurnRenderer` (an engine-started turn too); deltas/tools feed it; a
  2 s-ish ticker (`tokio::time::interval(500 ms)`) calls `poll` and sends/edits via the messenger
  (map page → ClientMsgId); `TurnEnded` → `finish` → idle activity → send queued prompt if any.
- **Activity:** working + `renderer.current_label()` sent when the label changes and refreshed every 15 s
  while a turn runs; waiting + card's waiting label while a card is open (wins over working); idle when the
  turn ends and no card is open. Activity errors are ignored (logged at debug).
- **Cards:** `BackendEvent::Request` → `cards::*_card` → `send_rich` (+ follow-ups via `send_rich`) →
  `CardBook::open` with deadline now+10 min; text-only questions are tracked the same way without a card
  in the CardBook (core keeps its own ordered list of open requests: {plan, responder, client_msg_id?, deadline});
  `MessageSent` → `CardBook::sent`; owner's answer → `answer_from_values` → responder; ticker expires →
  `timeout_answer` + reply "No answer; skipped"; `SendFailed` for a card's client_msg_id → `Answer::Decline` + reply
  "Could not show the card (<code>); the request was declined."; `/stop` or an interrupt → the backend
  declines open requests itself; the core closes its open cards when the turn ends with interrupted or on Exited.
- **Background:** `AgentMessage` → its own agent.reply@1 (title = label masked ≤120, text masked; long text paged);
  cursor saved to state. `TaskCompleted{background:true}` → `renderer.task_step(...)` if a turn runs, else a
  short reply "Task <description> <completed|failed|stopped>." (+ masked report first line ≤ 200 chars).
  `Error(msg)` → note in the current reply, or a short reply.
- **Messenger reconnect:** `ConnectionStateChanged(Connected)` after a non-connected state → send activity
  again and fetch pending messages (when the backend has AgentMessages).
- **SignedOut:** the core returns `BridgeError::SignedOut` (main re-signs in — Task 9). `DeviceRemoved` →
  `BridgeError::DeviceRemoved` (fatal).
- **Backend exit:** `Exited` → current renderer `note("(<name> stopped; restarting)")` + `finish(false)`;
  open cards closed locally (responders already dead; reply "Declined: <name> stopped."); restart after
  `Backoff::next_delay()` resuming the stored session for the folder; then a reply: "<name> restarted.
  Undelivered background messages and in-memory schedules were lost." (only the parts whose capabilities
  the backend has: AgentMessages / Schedules). Start failures retry with the backoff. Backoff resets when
  the backend ran 60 s.
- **Commands** (results as masked agent.reply@1 text; refused when the capability is missing: "Not supported by <name>."):
  `/new` → interrupt if busy, `start{folder, None, default mode}`, store new id, mode = default, reply "New session <id> in <folder>.";
  `/cd <path>` → `folders::resolve` (error → reply naming the error and listing the roots) → `start{new folder, state.sessions[new] , mode}`
  (SessionNotFound → new) → state.folder updated → reply; `/stop` → interrupt; `/status` → "Session: … Folder: … Model: …
  Permission mode: … (bypass shown clearly) State: busy|idle Running tasks: N <name>: <version>";
  `/compact` (idle only: busy → "Wait until the turn ends."); `/model [id]`; `/effort x`; `/mode default|acceptEdits|plan`
  → SetPermissionMode, core remembers; `/mode bypass` → `bypass_card`, Approve → SetPermissionMode(BypassPermissions) + reply,
  otherwise "Approvals stay on."; `/goal <text>`; `/sessions`; `/resume <id>` → `start{folder, Some(id)}` (SessionNotFound →
  "No session <id> in this folder."); `/fork` (idle only) → new id stored; `/tasks`; `/task stop <id>`; `/btw <q>`;
  `/schedule …`; `/skills`; `/help` → `commands::HELP`; Passthrough → as owner text prompt; Usage → reply usage.
  Backend errors → reply "<method> failed (<code>): <message>".
- State saved on every change (folder, sessions, cursor, engine instance).

**Integration tests (`crates/push-bridge/tests/bridge.rs`):**
1. With `TestBackend` + `TestMessenger`: owner prompt → backend `prompt`; emitted turn events → one reply
   card sent then edited, final edit at TurnEnded; permission request → approval card; owner answer
   (`MessageKind::Answer` naming the card's id after `MessageSent`) → responder got `Permission(true)`; activity
   went working → waiting → working → idle; engine-started turn (TurnStarted{Engine} with no prompt) shown;
   `AgentMessage` → its own reply titled with the label.
2. With `CodaBackend` + a scripted fake coda over duplex (reuse the FakeCoda helper — move it to a
   `#[doc(hidden)] pub mod testing` in the crate or into `tests/common/mod.rs`) + `TestMessenger`: the same
   flow end to end (prompt, reply card, permission → approval card → answered → `{allow:true}` frame seen by
   the fake, turn ends, idle).
3. Stranger gets "This agent is private." once per hour (advance paused time 61 min → again).
4. A card timeout → `{allow:false}` and "No answer; skipped"; a question timeout → -32800.
5. Busy → steer; rejected `turnEnding` → prompt after TurnEnded.
6. Backend exit → note, restart after 1 s with the stored session, restart reply; repeated failures back off 1,2,4 s.
7. `/cd` outside roots refused with the roots listed; `/mode bypass` card approve → SetPermissionMode(bypass) and
   `/status` shows it; `/new` resets to default.

- [ ] Steps: messenger + TestBackend → core in slices (gate/routing, turns/rendering, cards, commands,
  lifecycle), each slice tests-first and committed; integration tests; `cargo test -p push-bridge`; clippy.

---

### Task 9: binary, CLI, signer selection, logging, lifecycle scripts

**`src/cli.rs`** (hand-written parsing, no new deps):
```
push-agent-bridge [--dir <folder>] <command>
  keygen                 create the key if needed; print its storage and thumbprint
  thumbprint             print the thumbprint of the existing key (error when none)
  enroll --code <code>   enroll with the owner's one-time code, register this device
  run                    run the bridge (what the scheduled task runs)
  status                 print config path, key storage + thumbprint, device id, folder, sessions (no secrets)
  help | --help | -h
```
`--dir` defaults to `config::default_dir()`; config at `<dir>\config.json`, state at `<dir>\state.json`,
logs at `<dir>\logs\bridge.log`, software key at `<dir>\agent-key.p8` (only with `"storage": "software-file"`).
Tests for parsing.

**`src/signer.rs`:** `open_key(config, dir, create: bool) -> Result<Arc<dyn AgentKey>, SignerError>`:
Windows storage → `push_agent_key::cng::CngKey::open_or_create(name)` (or `open` when `create` is false);
on non-Windows → `SignerError{code:"unsupported", message:"Windows key storage is only available on Windows; use \"storage\": \"software-file\" for development"}`.
SoftwareFile → `SoftwareKey::load_or_create(dir.join("agent-key.p8"))` with a stderr warning that it is for development only.

**`src/main.rs` (`run`):** load config + validate; open the key (no create: missing key → "run keygen
first"); state file; `BridgeSecretStore`; `push_sdk::Client::open(Config{auth_url: api_url, socket_url, host,
database_path: ":memory:", allow_insecure_transport…}, secrets)`; if `state.enrolled_user_id` is None →
exit 2 with "This agent is not enrolled: run `push-agent-bridge enroll --code <code>` with a code from the agents page.";
`sign_in_with_agent_key(signer, audience, device_name, Platform::Desktop)`; errors: `token_invalid` →
same enroll hint (exit 2); `SdkError::Signer` → "the key provider failed: <code>" (exit 3); other → exit 1.
`connect()`; spawn a task forwarding `Events` into the core's channel; build `CodaBackend`; `Bridge::run`
with shutdown = Ctrl-C (`tokio::signal::ctrl_c`); on `BridgeError::SignedOut` sign in again (fresh
assertion) and rebuild the bridge loop, at most once per 30 s. `enroll`: open/create key, Client with the
same secret store, `enroll_with_agent_key`, save `enrolled_user_id`, print "Enrolled as <user id>; device <id>.
Key thumbprint <t>." The SDK client needs a tokio runtime? — `Client` owns its runtime; main uses
`#[tokio::main]` for the bridge itself.

**`src/logging.rs`:** `tracing_subscriber::fmt().json()` writing to `<dir>\logs\bridge.log` (append; open with
`OpenOptions`; rotate by renaming to `bridge.log.1` when > 10 MB at start), `with_current_span(false)`,
env filter `PUSH_BRIDGE_LOG` default `info`. Events log only names, codes, counts, durations: e.g.
`turn.started`, `turn.ended interrupted=…`, `card.opened kind=…`, `card.answered`, `card.expired`,
`backend.exited`, `backend.restarted delay_ms=…`, `owner_gate.refused`, `command name=…`. Add a test
that a full core run with secrets in every input writes no input text to the log: install a capturing
`tracing` subscriber (`tracing_subscriber::fmt().with_writer(<Vec buffer>)`) in one core test and assert the
log contains none of the secret/test strings.

**Scripts (`crates/push-bridge/scripts/install.ps1`, `uninstall.ps1`)**, PowerShell 5.1+ compatible:
- install: params `-Exe <path to push-agent-bridge.exe>` (required), `-Dir <state folder>` (optional,
  default `$env:LOCALAPPDATA\push-messenger\coda-bridge`), `-TaskName` (default `push-agent-bridge`,
  e.g. one per instance). Registers a Scheduled Task for the current user: trigger `AtLogOn -User $env:USERNAME`,
  action `Exe` with arguments `--dir "<Dir>" run` (quoted), principal `-LogonType Interactive -RunLevel Limited`,
  settings `-RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)
  -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew`; `-Force` replaces; starts it
  (`Start-ScheduledTask`) unless `-NoStart`. Refuses when `config.json` is missing (prints the expected path).
- uninstall: stops and unregisters the task (`-TaskName`), optional `-DeleteKey` that runs
  `<exe> --dir <Dir> keygen`? no — deletes the CNG key via .NET `[System.Security.Cryptography.CngKey]::Open(name, provider).Delete()`
  for both providers, after a confirmation unless `-Force`; optional `-DeleteState` removes the folder.
- Validate syntax locally: `pwsh -NoProfile -Command "[System.Management.Automation.Language.Parser]::ParseFile('…', [ref]$null, [ref]$errors); $errors"`
  → no errors. Do not actually register a task in tests.

- [ ] Steps: cli tests → implement main/signer/logging → `cargo run -p push-bridge -- help` and `status`
  against a temp dir → scripts → parse check → commit `feat(bridge): push-agent-bridge CLI, logging and logon task scripts`.

---

### Task 10: docs and release notes

- Create `docs/agent-bridge.md`: what it is; architecture (core vs backends, Coda first); requirements
  (Windows, Coda installed, push-messenger with `agent-key` enabled); **setup on Windows**: build/obtain
  the exe, write `config.json` (full example with every field), `push-agent-bridge keygen` (TPM key,
  thumbprint), create the agent on the agents page / `POST /v1/agents`, compare the thumbprint, `enroll --code`,
  `scripts\install.ps1`; several agents = several `--dir` folders + task names + key names; the **behaviour**
  (replies, cards, activity, background messages); **commands** table (every command); **security model**
  (owner gate, owner-only card answers checked by the bridge, non-exportable TPM key, assertions 120 s and
  single-use jti, masking list + caps, **history is stored on the server unencrypted** (message bodies are
  plaintext to the server), /mode bypass confirmation/reset/status, /cd real paths inside roots, no shell,
  logs without bodies/tokens, `CODA_SERVE_API_KEY` never set); **troubleshooting** (not enrolled, token_invalid,
  signer errors NTE_*, TPM unavailable → software KSP, Coda not found, restarts/backoff, logs location,
  "busy", session not found after moving folders); an opt-in live **smoke** checklist (the 5 steps of the request);
  how to add another backend (implement `Backend`, add a config value).
- `docs/agent-key-sign-in.md`: replace "The SDK and its bindings have no agent helpers" with the push-sdk API
  (`enroll_with_agent_key`, `sign_in_with_agent_key`, `AgentSigner`, `assemble_assertion`, `CardBook`), note C/Node
  bindings do not expose it yet.
- `docs/rich-messages.md`: built-ins list gains `agent.choose@1` and `agent.plan-approval@1` (definitions,
  what they're for); grep for "four built-in" / counts and fix.
- `README.md`: one line + link in the docs/components list.
- `CHANGELOG.md` under "Unreleased": Added — push-agent-bridge (crate push-bridge) with Coda backend; push-sdk
  agent-key sign-in through a signer and CardBook; push-agent-key signers; built-in templates agent.choose@1 and
  agent.plan-approval@1. Follow the file's existing style.
- [ ] Commit `docs: agent bridge setup, commands, security and troubleshooting`.

---

## Self-review notes

- Every Behaviour row → Task 8 (+6/7); Cards table → Task 6/8; Commands → Task 5/8; Error handling → Task 7/8/9;
  Security → Tasks 4/5/8/9/10; Testing list → tests in Tasks 2–9; Acceptance → all; templates → Task 1.
- Deviation to record in the PR: C/Node bindings not extended (not required); the live smoke test is
  documented, not run in CI.
