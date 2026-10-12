# Plan 3A — Conformance Suite and Set-Based Storage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prepare `push-domain` for PostgreSQL (Plan 3B) without touching a database: move the 199 domain scenarios and the storage contract into a new crate, `push-testkit`, written once and runnable against any `Store`; tighten the `Tx` contract where Postgres differs from the in-memory store (failed statements, device registration races, monotonic `updated_at`); add a per-conversation lock for group receipt positions; and replace every per-member and per-conversation loop over the store with set-based `Tx` methods that Postgres will answer in one statement each.

**Architecture:** `push-testkit` holds a generic `World<S: Store>`, the scenarios as private async functions generic over a `Backend` (a factory of fresh, empty stores) and one case list per module. A test target with `harness = false` hands a backend to `push_testkit::run`, which runs every case as a separately named libtest trial with its own tokio runtime. `push-testkit/tests/in_memory.rs` runs the suite against `InMemoryStore`; Plan 3B adds `push-store/tests/postgres.rs`, which runs the very same cases against Postgres. The in-memory store stays the reference implementation of every `Tx` method, including the new set-based ones. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §3.2, §4.3–§4.5, §12.

**Tech Stack:** Rust 2024 edition (toolchain ≥ 1.85), tokio, async-trait, chrono, uuid, thiserror, serde_json; new: `libtest-mimic` 0.8 (MSRV 1.65) for the custom test harness.

---

## Before you start

- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-03a-conformance
```

- The toolchain is rustup-managed (`C:\Users\yurio\.cargo\bin`), with the `1.85` toolchain
  installed for the MSRV check (`rustup toolchain list`). If `cargo --version` prints
  something older than 1.85, or `cargo clippy` is missing, the shell has a stale PATH. Fix it
  for the current PowerShell session:

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
```

- **Edit files with the edit tool, and keep them LF.** The repository stores text with LF
  line endings (`.gitattributes`: `* text=auto eol=lf`). Change existing files with the
  edit tool, which keeps their line endings; never rewrite a file through PowerShell
  `Set-Content` or `Out-File`. Some file-creation tools on Windows write CRLF: convert a
  new file to LF if one did. Git normalises line endings on commit either way. The Python
  helper in Task 1 writes LF itself.
- **Do not pipe `cargo fmt`.** Run `cargo fmt --all -- --check` on its own and read the
  exit code; piping it into `Select-String` or `Out-Null` hides the diff and the failure.
  If it reports a diff, run `cargo fmt --all` and check again. The code in this plan is
  already formatted, so a diff usually means a copying mistake.
- **Docker** is not used by this plan; Plan 3B's Postgres tests need it running. Python 3 is
  needed for the conversion script in Tasks 1 and 2 (`python --version`).
- Baseline: `cargo test --workspace` passes 247 tests (48 unit, 199 integration).
- **Testing rule:** every test in this plan is written before the code it tests, and is run
  once to watch it fail. The exceptions are marked: Tasks 1–3 move existing tests and pin
  behaviour the code already has.
- **Every task ends green.** The last step of each task runs all four gates:

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

  The last one is what the `msrv` CI job needs; it uses the `Cargo.lock` that the first
  three commands updated. Never add `#[allow(dead_code)]` (this plan removes the only one,
  in the old `tests/common/mod.rs`). The tasks are ordered so that every helper is used by
  the task that introduces it.
- **Scope rule:** no Postgres code in this plan, and no behaviour change visible through
  `Messaging` except the device registration race (Task 4). Every scenario keeps passing
  unchanged.

## Key rules the code must preserve (from the spec)

- `seq` is gapless per conversation; every failure path rolls back.
- Never acknowledge before commit. Events are published only after commit.
- Positions only move forward. Delivery positions move only on `ack`.
- A block is never revealed to the blocked user.
- Per-member positions in a group are private: only group-wide minimums leave the domain.
- The in-memory store stays strict and is the reference implementation: every `Tx`
  method's documented behaviour is what it implements, and every contract case passes on
  it.

## Decisions made for this plan

These were settled while writing the plan. Do not reopen them during execution.

1. **Conformance design: a separate crate, `push-testkit`, with a custom libtest harness.**
   - `crates/push-testkit` (`publish = false`) holds `World<S>` (the old
     `tests/common/mod.rs`, generic over the store), the domain scenarios (one module per old
     test file), the `Tx` contract cases (the old `memory.rs` unit tests, plus new ones) and
     `run`. A scenario is a private `async fn name<B: Backend>(backend: &B)`; `Backend` makes
     fresh, empty, isolated stores (`fn store(&self) -> impl Future<Output = Self::Store>`)
     and may clean up after a passing case (`finish`).
   - Each module ends with `pub(crate) fn cases<B: Backend>() -> Vec<Case<B>>` listing its
     scenarios through the `case_list!` macro. Because the scenario functions are private and
     only the lists reach them, **a scenario missing from its list is dead code and fails the
     `clippy -D warnings` gate**: the lists cannot drift from the code.
   - `run(backend, cases)` turns each case into a `libtest_mimic::Trial` named
     `module::scenario` (for example `send::a_retry_returns_the_original_and_leaves_no_gap`)
     that builds its own two-worker tokio runtime. libtest-mimic gives the usual test
     output, name filters, `--list`, parallel threads and caught panics.
   - Test targets use `harness = false`: `push-testkit/tests/in_memory.rs` (this plan) and
     `push-store/tests/postgres.rs` (Plan 3B). The Postgres binary owns its container in
     `main`, starts it on first use and removes it when the run ends.
   - Rejected: a `testkit` feature on `push-domain` (its own integration tests would need a
     self dev-dependency to enable it, and the scenarios would ship inside the domain crate);
     `push-domain` dev-depending on a testkit crate (a dev-dependency cycle: legal for
     integration tests, but confusing, and unit tests would see a second copy of every
     `push-domain` type); a `macro_rules!` generating one `#[tokio::test]` per scenario in
     each consumer (a hand-maintained list of ~250 names that nothing checks, and a
     `#[tokio::test]` cannot own a container for the whole run without leaking it from a
     `static`).
   - `push-domain/tests` disappears; `push-domain` keeps its unit tests (22 after Task 3)
     and loses its tokio dev-dependency.
2. **A failed `Tx` method may leave its transaction unusable.** On Postgres every statement
   after an error fails until rollback. The contract says so, no domain code continues after
   an error (it already returns through `?`), and every contract case that expects a failure
   runs it in a transaction of its own.
3. **Device registration is race-free.** `insert_device` returns whether the device is now an
   active device of the user (Postgres: `INSERT … ON CONFLICT DO NOTHING`, which waits for a
   concurrent insert of the same id, then a check). `register_device` uses that instead of
   check-then-insert; `device_exists` is removed from `Tx`.
4. **`next_seq` never moves `updated_at` back** (`GREATEST` on Postgres), so a replica with a
   slightly slow clock cannot hide a conversation from `changed_since`.
5. **Group receipt positions are serialised per conversation.** New
   `Tx::lock_conversation_positions` (Postgres: `pg_advisory_xact_lock` on a hash of the
   conversation id; in memory: nothing, the store-wide lock already serialises). Every
   operation that compares group-wide positions before and after a change takes it before
   reading "before", through `read::group_positions_before`: `read_up_to`, `ack`,
   `prepare_append` (sends, edits, deletes) and `begin_change` (group changes). Appends take
   it after `next_seq`; acks and reads take only this lock and never the conversation row
   lock, so they do not wait for appends. Without it, two members acking at the same time
   under READ COMMITTED each see the other's old position and the group receipt is lost
   (Plan 3B proves this with a concurrency test that fails when the lock is removed).
6. **Set-based `Tx` methods** replace every loop over the store in the domain (the reviewers'
   must-dos), each with a written contract, an in-memory reference implementation and
   contract cases:
   - `group_positions(conversation, last_seq) -> Option<GroupPositions>`: the group-wide
     minimums in one call. `GroupPositions` moves from `read.rs` to `model.rs` and becomes
     public. The effective delivered position is computed from
     `max(delivered_seq, read_seq)`, which equals the old "effective delivered, at least the
     effective read" because effective positions only grow with the raw position.
   - `member_positions(conversation) -> Vec<(Member, i64)>`: every member with their
     `delivered_seq`, for `conversation_state` and direct receipts.
   - `sync_positions(user, device) -> Vec<(Conversation, Member, i64)>`: the memberships
     with something left to sync, each with its start position (floor-clamped, missing
     position = floor), so `sync` no longer reads a cursor per membership.
   - `conversation_overviews(user, changed_since, unread_cap) -> Vec<ConversationOverview>`:
     everything a listing needs per membership (current members and admins, the direct
     peer with their delivery position, the unread count, group-wide positions, and for a
     former member the time of the entry at `left_at_seq`, a point lookup). The listing is
     now two store calls (overviews and `contacts_of`) however many conversations there
     are. The `changed_since` filter on `updated_at` moves into the store; the former
     member's own filter stays in the domain.
7. **Remaining per-row loops are deferred** (see Deferred): `register_device` creates one
   position per membership, and `join` (`create_group`, `add_members`) one per device of
   each new member. They run on rare operations.

## File structure

```
Cargo.toml                    + push-testkit member; libtest-mimic, push-domain
crates/push-domain/
├── Cargo.toml                no dev-dependencies any more
├── src/
│   ├── lib.rs                + GroupPositions, ConversationOverview re-exports
│   ├── model.rs              + GroupPositions (moved from read.rs), ConversationOverview
│   ├── store.rs              contract: failed methods, insert_device -> bool,
│   │                           device_exists removed, monotonic updated_at,
│   │                           + lock_conversation_positions, group_positions,
│   │                           member_positions, sync_positions, conversation_overviews
│   ├── memory.rs             the new methods; State helpers; unit tests moved out
│   ├── service.rs            register_device through insert_device
│   ├── read.rs               group_positions_before, visible_group_positions;
│   │                           effective_position moved into the store
│   ├── sync.rs, send.rs,
│   │   groups.rs             position lock; sync through sync_positions
│   └── conversations.rs      listing through conversation_overviews
└── tests/                    DELETED (moved to push-testkit)
crates/push-testkit/          NEW (publish = false)
├── Cargo.toml
├── src/
│   ├── lib.rs                Backend, Case, case_list!, cases(), run()
│   ├── world.rs              World<S>, Person and helpers (was tests/common/mod.rs)
│   ├── scenarios/            the 17 old test files, one module each, + mod.rs
│   └── contract/             the Tx contract: transactions, users, contacts,
│                               conversations, entries, positions, overviews
└── tests/in_memory.rs        harness = false: the suite on InMemoryStore
```

---

### Task 1: The testkit crate and its first scenarios

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/push-testkit/Cargo.toml`
- Create: `crates/push-testkit/src/lib.rs`
- Create: `crates/push-testkit/src/world.rs` (from `crates/push-domain/tests/common/mod.rs`)
- Create: `crates/push-testkit/src/scenarios/mod.rs`
- Move: `crates/push-domain/tests/devices.rs` → `crates/push-testkit/src/scenarios/devices.rs`
- Create: `crates/push-testkit/tests/in_memory.rs`
- Create (not committed): `target/plan-03a/convert_scenarios.py`

This task builds the harness and moves the smallest test file through it. The tests pin
behaviour the code already has.

- [ ] **Step 1: Add the crate to the workspace**

```powershell
New-Item -ItemType Directory crates\push-testkit\src\scenarios, crates\push-testkit\tests, target\plan-03a -Force
```

In `Cargo.toml`, replace `members = ["crates/push-domain"]` with:

```toml
members = ["crates/push-domain", "crates/push-testkit"]
```

and add these two lines to `[workspace.dependencies]`, after the `chrono` line:

```toml
libtest-mimic = "0.8"
push-domain = { path = "crates/push-domain" }
```

Create `crates/push-testkit/Cargo.toml`:

```toml
[package]
name = "push-testkit"
description = "The push-domain behaviour suite, written once and run against every Store."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
async-trait.workspace = true
chrono.workspace = true
libtest-mimic.workspace = true
push-domain.workspace = true
tokio = { workspace = true, features = ["rt-multi-thread", "time"] }

[[test]]
name = "in_memory"
harness = false

[lints]
workspace = true
```

- [ ] **Step 2: Write the harness**

Create `crates/push-testkit/src/lib.rs`:

```rust
//! The behaviour suite of `push-domain`, written once and run against every [`Store`].
//!
//! Each scenario is an async function generic over a [`Backend`], which makes a fresh,
//! empty store for it. A test target with `harness = false` passes a backend to [`run`],
//! which runs every [`Case`] as a separately named test with its own tokio runtime, in
//! parallel. `tests/in_memory.rs` runs the suite against the in-memory store;
//! `push-store` runs the same cases against PostgreSQL.
//!
//! The scenario functions are private to this crate and reachable only through the case
//! lists, so a scenario missing from its list is dead code, which the workspace's
//! `clippy -D warnings` gate rejects.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use libtest_mimic::{Arguments, Conclusion, Trial};
use push_domain::Store;

mod scenarios;
mod world;

pub use world::{Person, Svc, World, bodies, delete, edit, entries_for, seqs, system_events, text};

/// Makes stores for the scenarios.
pub trait Backend: Clone + Send + Sync + 'static {
    type Store: Store + Clone + 'static;

    /// A new store holding no data, isolated from every other store.
    fn store(&self) -> impl Future<Output = Self::Store>;

    /// Called after a case passed, to release what [`Backend::store`] made. A failed case
    /// skips it; the test binary cleans up when it ends.
    fn finish(&self) -> impl Future<Output = ()> {
        async {}
    }
}

/// One named scenario.
pub struct Case<B> {
    pub name: String,
    run: fn(B) -> Pin<Box<dyn Future<Output = ()>>>,
}

impl<B> Case<B> {
    fn new(name: &str, run: fn(B) -> Pin<Box<dyn Future<Output = ()>>>) -> Self {
        Self {
            name: name.to_owned(),
            run,
        }
    }

    /// Prefixes the name with the module that holds the scenario.
    fn within(mut self, module: &str) -> Self {
        self.name = format!("{module}::{}", self.name);
        self
    }
}

/// The cases for a list of scenario functions in the current module.
macro_rules! case_list {
    ($($scenario:ident),* $(,)?) => {
        vec![$(
            $crate::Case::new(stringify!($scenario), |backend| {
                Box::pin(async move { $scenario(&backend).await })
            }),
        )*]
    };
}
pub(crate) use case_list;

/// Every case that any store must pass.
pub fn cases<B: Backend>() -> Vec<Case<B>> {
    scenarios::cases()
}

/// Runs `cases` as the tests of this binary and exits. Each case gets a backend from
/// `backend` and a multi-threaded tokio runtime of its own.
pub fn run<B: Backend>(
    backend: impl Fn() -> B + Send + Sync + 'static,
    cases: Vec<Case<B>>,
) -> Conclusion {
    let backend = Arc::new(backend);
    let trials = cases
        .into_iter()
        .map(|case| {
            let backend = Arc::clone(&backend);
            Trial::test(case.name, move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("a tokio runtime");
                let backend = backend();
                runtime.block_on(async {
                    (case.run)(backend.clone()).await;
                    backend.finish().await;
                });
                Ok(())
            })
        })
        .collect();
    libtest_mimic::run(&Arguments::from_args(), trials)
}
```

Create `crates/push-testkit/src/scenarios/mod.rs`:

```rust
//! The scenarios, one module per area, and their case lists.

use crate::{Backend, Case};

mod devices;

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    let modules: Vec<(&str, Vec<Case<B>>)> = vec![("devices", devices::cases())];
    modules
        .into_iter()
        .flat_map(|(module, cases)| cases.into_iter().map(move |case| case.within(module)))
        .collect()
}
```

Create `crates/push-testkit/tests/in_memory.rs`:

```rust
//! The behaviour suite against the in-memory store.

use push_domain::InMemoryStore;
use push_testkit::Backend;

#[derive(Debug, Clone, Copy)]
struct InMemory;

impl Backend for InMemory {
    type Store = InMemoryStore;

    async fn store(&self) -> InMemoryStore {
        InMemoryStore::new()
    }
}

fn main() {
    push_testkit::run(|| InMemory, push_testkit::cases()).exit();
}
```

- [ ] **Step 3: Make the world generic over its store**

Copy (do not move: the other test files still use it until Task 2) the old harness:

```powershell
Copy-Item crates\push-domain\tests\common\mod.rs crates\push-testkit\src\world.rs
```

In `crates/push-testkit/src/world.rs`, replace everything from the first line through the `pub type Svc = …;` line with:

```rust
//! The scenarios' world: a `Messaging` service over a backend's store, a manual clock and
//! a recording notifier.

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use push_domain::{
    ClientMsgId, Conversation, ConversationId, DeleteMessage, DeviceId, EditText, Entry, Event,
    ManualClock, Member, MessageRef, Messaging, Policy, RecordingNotifier, SendReceipt, SendText,
    Store, SystemEvent, Tx, UserId,
};

use crate::Backend;

pub type Svc<S> = Messaging<S, Arc<ManualClock>, Arc<RecordingNotifier>>;
```

replace the `World` struct with:

```rust
pub struct World<S> {
    pub svc: Svc<S>,
    pub store: S,
    pub clock: Arc<ManualClock>,
    pub notifier: Arc<RecordingNotifier>,
}
```

and replace the start of its `impl` block, from `impl World {` through `let store = InMemoryStore::new();`, with:

```rust
impl<S: Store + Clone> World<S> {
    /// A world over a new store from `backend`, with the default policy.
    pub async fn new<B: Backend<Store = S>>(backend: &B) -> Self {
        Self::with_policy(backend, Policy::default()).await
    }

    pub async fn with_policy<B: Backend<Store = S>>(backend: &B, policy: Policy) -> Self {
        let clock = Arc::new(ManualClock::new(
            Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap(),
        ));
        let notifier = Arc::new(RecordingNotifier::default());
        let store = backend.store().await;
```

Nothing else in the file changes; the `#![allow(dead_code)]` is gone because a library's public items are never dead.

- [ ] **Step 4: Save the conversion script**

Every old test file becomes a scenario module by the same mechanical change: tests become private generic functions, `World::new()` takes the backend, helpers that build or use a world become generic, and a case list goes at the end. Create `target/plan-03a/convert_scenarios.py` (`target/` is ignored by git; the script is a tool, not part of the repository):

```python
"""Turns a push-domain integration test file into a push-testkit scenario module.

Usage: python convert_scenarios.py SOURCE.rs TARGET.rs
"""

import re
import sys

source, target = sys.argv[1], sys.argv[2]
text = open(source, encoding="utf-8").read()

# The harness now lives at the crate root.
text = text.replace("mod common;\n\n", "", 1)
text = text.replace("use common::", "use crate::", 1)

# Every test becomes a private scenario function taking the backend.
names = re.findall(r"#\[tokio::test\]\nasync fn (\w+)\(\)", text)
text = re.sub(
    r"#\[tokio::test\]\nasync fn (\w+)\(\)",
    r"async fn \1<B: Backend>(backend: &B)",
    text,
)

# Worlds come from the backend.
text = text.replace("World::new()", "World::new(backend).await")
text = re.sub(
    r"World::with_policy\((.*?)\);",
    r"World::with_policy(backend, \1).await;",
    text,
    flags=re.DOTALL,
)

# Helpers that build a world take the backend first, and so do their callers.
for name, params in re.findall(r"async fn (\w+)\(([^)]*)\)\s*-> \(World,", text, flags=re.DOTALL):
    backend_params = f"backend: &B, {params.strip()}" if params.strip() else "backend: &B"
    text = re.sub(
        rf"async fn {name}\([^)]*\)(\s*)-> \(World,",
        lambda match: f"async fn {name}<B: Backend>({backend_params}){match.group(1)}-> (World<B::Store>,",
        text,
    )
    text = text.replace(f"{name}()", f"{name}(backend)")
    text = re.sub(rf"\b{name}\((?!backend)", f"{name}(backend, ", text)

# Helpers that use a world are generic over its store.
uses_store = False


def generic_helper(match):
    global uses_store
    uses_store = True
    params = match.group(3).replace("&World", "&World<S>")
    return f"{match.group(1)}fn {match.group(2)}<S: Store>({params})"


text = re.sub(
    r"((?:async )?)fn (\w+)\(([^)]*&World\b(?!<)[^)]*)\)",
    generic_helper,
    text,
)

def add_to_use(text, path, item):
    """Adds `item` to the first `use {path}::…;`, or adds a new `use` line."""
    match = re.search(rf"use {path}::(\{{[^}}]*\}}|\w+);", text)
    if match is None:
        first_use = text.index("use ")
        return text[:first_use] + f"use {path}::{item};\n" + text[first_use:]
    items = match.group(1).strip("{}")
    if re.search(rf"\b{item}\b", items):
        return text
    merged = f"use {path}::{{{item}, {items.strip()}}};"
    return text[: match.start()] + merged + text[match.end() :]


for item in ["case_list", "Case", "Backend"]:
    text = add_to_use(text, "crate", item)
if uses_store:
    text = add_to_use(text, "push_domain", "Store")

listed = ",\n        ".join(names)
text = text.rstrip("\n") + (
    "\n\npub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {\n"
    f"    case_list![\n        {listed},\n    ]\n}}\n"
)
open(target, "w", encoding="utf-8", newline="\n").write(text)
print(f"{target}: {len(names)} scenarios")
```

- [ ] **Step 5: Move the devices tests**

```powershell
git mv crates/push-domain/tests/devices.rs crates/push-testkit/src/scenarios/devices.rs
python target/plan-03a/convert_scenarios.py crates/push-testkit/src/scenarios/devices.rs crates/push-testkit/src/scenarios/devices.rs
cargo fmt --all
```

Expected output: `crates/push-testkit/src/scenarios/devices.rs: 7 scenarios`. The file is now:

```rust
use crate::{Backend, Case, World, case_list};
use push_domain::{DeviceId, DomainError, UserId};

async fn registering_a_device_for_an_unknown_user_fails<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let result = world
        .svc
        .register_device(DeviceId::new(), UserId::new())
        .await;
    assert_eq!(result, Err(DomainError::UnknownUser));
}

async fn identify_accepts_the_users_own_active_device<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}

async fn identify_rejects_a_device_of_another_user<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let bob = world.person().await;
    assert_eq!(
        world.svc.identify(bob.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn identify_rejects_a_revoked_device<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    world.svc.revoke_device(alice.device).await.unwrap();
    assert_eq!(
        world.svc.identify(alice.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn a_device_id_of_another_user_cannot_be_registered<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    let bob = world.person().await;
    assert_eq!(
        world.svc.register_device(alice.device, bob.user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn a_revoked_device_cannot_be_registered_again<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    world.svc.revoke_device(alice.device).await.unwrap();
    assert_eq!(
        world.svc.register_device(alice.device, alice.user).await,
        Err(DomainError::UnknownDevice)
    );
}

async fn registering_the_same_user_and_device_twice_is_harmless<B: Backend>(backend: &B) {
    let world = World::new(backend).await;
    let alice = world.person().await;
    world.svc.register_user(alice.user).await.unwrap();
    world
        .svc
        .register_device(alice.device, alice.user)
        .await
        .unwrap();
    assert_eq!(world.svc.identify(alice.device, alice.user).await, Ok(()));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        registering_a_device_for_an_unknown_user_fails,
        identify_accepts_the_users_own_active_device,
        identify_rejects_a_device_of_another_user,
        identify_rejects_a_revoked_device,
        a_device_id_of_another_user_cannot_be_registered,
        a_revoked_device_cannot_be_registered_again,
        registering_the_same_user_and_device_twice_is_harmless,
    ]
}
```

- [ ] **Step 6: Run the suite**

```powershell
cargo test -p push-testkit
```

Expected: the `in_memory` target runs 7 tests named `devices::…`, all `ok`.

Check the dead-code guard once: delete the line `identify_rejects_a_revoked_device,` from the list in `devices.rs` and run `cargo clippy -p push-testkit --all-targets -- -D warnings`. Expected: `error: function `identify_rejects_a_revoked_device` is never used`. Put the line back.

- [ ] **Step 7: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 247 tests pass (`push-domain`: 48 unit and 192 integration; `push-testkit` `in_memory`: 7), no warnings, no formatting diff, the 1.85 build succeeds.

```powershell
git add Cargo.toml Cargo.lock crates
git commit -m "test: add push-testkit with the devices scenarios"
```

---

### Task 2: Move every domain scenario into the testkit

**Files:**
- Move: the other 16 files of `crates/push-domain/tests/` → `crates/push-testkit/src/scenarios/`
- Delete: `crates/push-domain/tests/common/mod.rs`
- Modify: `crates/push-testkit/src/scenarios/mod.rs`
- Modify: `crates/push-testkit/src/scenarios/send.rs` (one manual change)

The tests pin behaviour the code already has.

- [ ] **Step 1: Convert the files**

```powershell
foreach ($file in Get-ChildItem crates\push-domain\tests\*.rs) {
    $name = $file.Name
    git mv "crates/push-domain/tests/$name" "crates/push-testkit/src/scenarios/$name"
    python target/plan-03a/convert_scenarios.py "crates/push-testkit/src/scenarios/$name" "crates/push-testkit/src/scenarios/$name"
}
git rm -r crates/push-domain/tests/common
```

Expected output, one line per file: contacts 24, delete 8, direct 6, edit 19, group_receipts 5, group_settings 7, groups 18, history 11, invites 14, listings 11, membership 11, read_state 12, receipts 10, send 14, sync 15, visibility 7 scenarios (192 in total). `crates/push-domain/tests` no longer exists.

- [ ] **Step 2: List the modules**

Replace `crates/push-testkit/src/scenarios/mod.rs` with:

```rust
//! The scenarios, one module per area, and their case lists.

use crate::{Backend, Case};

mod contacts;
mod delete;
mod devices;
mod direct;
mod edit;
mod group_receipts;
mod group_settings;
mod groups;
mod history;
mod invites;
mod listings;
mod membership;
mod read_state;
mod receipts;
mod send;
mod sync;
mod visibility;

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    let modules: Vec<(&str, Vec<Case<B>>)> = vec![
        ("contacts", contacts::cases()),
        ("delete", delete::cases()),
        ("devices", devices::cases()),
        ("direct", direct::cases()),
        ("edit", edit::cases()),
        ("group_receipts", group_receipts::cases()),
        ("group_settings", group_settings::cases()),
        ("groups", groups::cases()),
        ("history", history::cases()),
        ("invites", invites::cases()),
        ("listings", listings::cases()),
        ("membership", membership::cases()),
        ("read_state", read_state::cases()),
        ("receipts", receipts::cases()),
        ("send", send::cases()),
        ("sync", sync::cases()),
        ("visibility", visibility::cases()),
    ];
    modules
        .into_iter()
        .flat_map(|(module, cases)| cases.into_iter().map(move |case| case.within(module)))
        .collect()
}
```

- [ ] **Step 3: Make the commit probe generic**

`send::events_are_published_only_after_the_send_commits` builds its own service over an `InMemoryStore`. In `crates/push-testkit/src/scenarios/send.rs`, replace everything from `use push_domain::{` down to (not including) `async fn messages_get_consecutive_seq_numbers_starting_at_one` with:

```rust
use push_domain::{
    Clock, ConversationId, DeviceId, DomainError, Event, ManualClock, Messaging, Notifier,
    SendText, Store, Tx, UserId, ValidationError,
};
use std::sync::Arc;

/// Proves events are published after commit: it begins its own transaction (which would
/// wait forever on the in-memory store's lock if the sender's were still open) and checks
/// the new message is already visible.
struct CommitProbe<S> {
    store: S,
    observed: std::sync::Mutex<Vec<i64>>,
}

#[async_trait::async_trait]
impl<S: Store> Notifier for CommitProbe<S> {
    async fn publish(&self, events: &[Event]) {
        let Some(Event::EntriesAvailable { users }) = events.first() else {
            return;
        };
        let mut tx = tokio::time::timeout(std::time::Duration::from_secs(2), self.store.begin())
            .await
            .expect("published while the sending transaction was still open")
            .unwrap();
        // The conversation's last_seq as seen by an independent transaction.
        let memberships = tx.memberships(users[0]).await.unwrap();
        let mut observed = self.observed.lock().unwrap();
        observed.extend(
            memberships
                .iter()
                .map(|(conversation, _)| conversation.last_seq),
        );
    }
}

async fn register<S: Store>(svc: &Messaging<S, Arc<ManualClock>, Arc<CommitProbe<S>>>) -> Person {
    let person = Person {
        user: UserId::new(),
        device: DeviceId::new(),
    };
    svc.register_user(person.user).await.unwrap();
    svc.register_device(person.device, person.user)
        .await
        .unwrap();
    person
}

async fn events_are_published_only_after_the_send_commits<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let probe = Arc::new(CommitProbe {
        store: store.clone(),
        observed: std::sync::Mutex::new(Vec::new()),
    });
    let clock = Arc::new(ManualClock::new(
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap(),
    ));
    let svc = Messaging::new(store, clock, Arc::clone(&probe));
    let alice = register(&svc).await;
    let bob = register(&svc).await;
    svc.contact_request(alice.user, bob.user).await.unwrap();
    svc.contact_accept(bob.user, alice.user).await.unwrap();
    let chat = svc.open_direct(alice.user, bob.user).await.unwrap();

    svc.send_text(text(&alice, chat, "hi")).await.unwrap();
    assert_eq!(*probe.observed.lock().unwrap(), [1]);
}
```

- [ ] **Step 4: Run the suite**

```powershell
cargo fmt --all
cargo test --workspace
```

Expected: `push-domain` runs only its 48 unit tests; `in_memory` runs 199 tests, all `ok`.

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 247 tests pass (48 unit, 199 `in_memory`), no warnings, no diff, the 1.85 build succeeds. `git diff --cached -M --stat` shows the 16 files as renames.

```powershell
git add -A crates
git commit -m "test: run the domain scenarios through push-testkit"
```

---

### Task 3: The storage contract as conformance cases

**Files:**
- Create: `crates/push-testkit/src/contract/mod.rs`, `transactions.rs`, `users.rs`, `contacts.rs`, `conversations.rs`, `entries.rs`, `positions.rs`
- Modify: `crates/push-testkit/src/lib.rs`
- Modify: `crates/push-domain/src/memory.rs` (the `tests` module moves out)
- Modify: `crates/push-domain/src/store.rs` (doc)
- Modify: `crates/push-domain/Cargo.toml`

The 26 unit tests of `memory.rs` become contract cases any store must pass, and new ones
cover every `Tx` method they missed (users, devices, contacts, members, memberships, read
positions, delivered positions). Every expected failure runs in its own transaction
(decision 2). These tests pin behaviour the in-memory store already has.

- [ ] **Step 1: Write the contract cases**

```powershell
New-Item -ItemType Directory crates\push-testkit\src\contract -Force
```

Create `crates/push-testkit/src/contract/mod.rs`:

```rust
//! The `Tx` contract (see `push_domain::store`), checked method by method.
//!
//! A method that fails may leave its transaction unusable (on Postgres, every later
//! statement fails), so every expected failure here ends its transaction.

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use push_domain::{
    ClientMsgId, ContentType, Conversation, ConversationId, ConversationKind, Entry, EntryKind,
    InviteRecord, Member, MessageId, Role, Store, Tx, UserId,
};

use crate::{Backend, Case};

mod contacts;
mod conversations;
mod entries;
mod positions;
mod transactions;
mod users;

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    let modules: Vec<(&str, Vec<Case<B>>)> = vec![
        ("contract::transactions", transactions::cases()),
        ("contract::users", users::cases()),
        ("contract::contacts", contacts::cases()),
        ("contract::conversations", conversations::cases()),
        ("contract::entries", entries::cases()),
        ("contract::positions", positions::cases()),
    ];
    modules
        .into_iter()
        .flat_map(|(module, cases)| cases.into_iter().map(move |case| case.within(module)))
        .collect()
}

fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

/// A direct conversation without a direct key, created by a user who need not exist.
fn conversation() -> Conversation {
    Conversation {
        id: ConversationId::new(),
        kind: ConversationKind::Direct,
        direct_key: None,
        title: None,
        created_by: UserId::new(),
        last_seq: 0,
        created_at: start(),
        updated_at: start(),
    }
}

/// A text message at `seq` from a sender who need not exist.
fn entry(conversation: ConversationId, seq: i64, client_msg_id: ClientMsgId) -> Entry {
    Entry {
        conversation_id: conversation,
        seq,
        message_id: MessageId::new(),
        client_msg_id,
        kind: EntryKind::Message,
        target_seq: None,
        sender_id: UserId::new(),
        sender_device_id: None,
        content_type: ContentType::Text,
        body: Some(b"hi".to_vec()),
        client_ts: None,
        server_ts: start(),
        edited_at: None,
        edit_seq: None,
        deleted_at: None,
    }
}

/// A current member who sees the conversation from its start.
fn member(conversation: ConversationId, user: UserId) -> Member {
    Member {
        conversation_id: conversation,
        user_id: user,
        role: Role::Member,
        visible_from_seq: 1,
        left_at_seq: None,
        read_seq: 0,
    }
}

fn invite(code: &str, user: UserId) -> InviteRecord {
    InviteRecord {
        code: code.into(),
        user_id: user,
        expires_at: start() + TimeDelta::days(1),
        max_uses: 1,
        uses: 0,
        revoked_at: None,
    }
}

fn seqs(entries: &[Entry]) -> Vec<i64> {
    entries.iter().map(|entry| entry.seq).collect()
}

/// A new store holding `conversation`.
async fn store_with<B: Backend>(backend: &B, conversation: Conversation) -> B::Store {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(conversation).await.unwrap();
    tx.commit().await.unwrap();
    store
}

/// Records these users.
async fn add_users<S: Store>(store: &S, users: &[UserId]) {
    let mut tx = store.begin().await.unwrap();
    for user in users {
        tx.insert_user(*user).await.unwrap();
    }
    tx.commit().await.unwrap();
}
```

Create `crates/push-testkit/src/contract/transactions.rs`:

```rust
use push_domain::{Store, Tx, UserId};

use crate::{Backend, Case, case_list};

async fn committed_changes_are_visible_to_later_transactions<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    let mut tx = store.begin().await.unwrap();
    tx.insert_user(user).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert!(tx.user_exists(user).await.unwrap());
}

async fn rolled_back_and_dropped_transactions_leave_no_trace<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (rolled_back, dropped) = (UserId::new(), UserId::new());

    let mut tx = store.begin().await.unwrap();
    tx.insert_user(rolled_back).await.unwrap();
    tx.rollback().await.unwrap();
    {
        let mut tx = store.begin().await.unwrap();
        tx.insert_user(dropped).await.unwrap();
    }

    let mut tx = store.begin().await.unwrap();
    assert!(!tx.user_exists(rolled_back).await.unwrap());
    assert!(!tx.user_exists(dropped).await.unwrap());
}

async fn a_transaction_sees_its_own_changes<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    let mut tx = store.begin().await.unwrap();
    tx.insert_user(user).await.unwrap();

    assert!(tx.user_exists(user).await.unwrap());
    tx.rollback().await.unwrap();
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        committed_changes_are_visible_to_later_transactions,
        rolled_back_and_dropped_transactions_leave_no_trace,
        a_transaction_sees_its_own_changes,
    ]
}
```

Create `crates/push-testkit/src/contract/users.rs`:

```rust
use push_domain::{DeviceId, DomainError, Store, Tx, UserId};

use super::add_users;
use crate::{Backend, Case, case_list};

async fn inserting_a_user_twice_is_harmless<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    let mut tx = store.begin().await.unwrap();
    tx.insert_user(user).await.unwrap();
    tx.insert_user(user).await.unwrap();

    assert!(tx.user_exists(user).await.unwrap());
    assert!(!tx.user_exists(UserId::new()).await.unwrap());
}

async fn revoked_devices_have_no_owner_and_are_not_listed<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (user, other) = (UserId::new(), UserId::new());
    add_users(&store, &[user, other]).await;
    let (kept, revoked, others) = (DeviceId::new(), DeviceId::new(), DeviceId::new());
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(kept, user).await.unwrap();
    tx.insert_device(revoked, user).await.unwrap();
    tx.insert_device(others, other).await.unwrap();
    tx.revoke_device(revoked).await.unwrap();

    assert_eq!(tx.device_owner(kept).await.unwrap(), Some(user));
    assert_eq!(tx.device_owner(revoked).await.unwrap(), None);
    assert_eq!(tx.device_owner(DeviceId::new()).await.unwrap(), None);
    assert_eq!(tx.devices_of_user(user).await.unwrap(), vec![kept]);
}

async fn inserting_a_known_device_id_changes_nothing<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (user, other) = (UserId::new(), UserId::new());
    add_users(&store, &[user, other]).await;
    let device = DeviceId::new();
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(device, user).await.unwrap();
    tx.insert_device(device, other).await.unwrap();

    assert_eq!(tx.device_owner(device).await.unwrap(), Some(user));
    assert!(tx.devices_of_user(other).await.unwrap().is_empty());
    assert!(tx.device_exists(device).await.unwrap());
    tx.revoke_device(device).await.unwrap();
    assert!(tx.device_exists(device).await.unwrap());
    assert!(!tx.device_exists(DeviceId::new()).await.unwrap());
}

async fn a_device_of_an_unknown_user_is_refused<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_device(DeviceId::new(), UserId::new()).await,
        Err(DomainError::Store(_))
    ));
}

async fn revoking_an_unknown_device_does_nothing<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.revoke_device(DeviceId::new()).await, Ok(()));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        inserting_a_user_twice_is_harmless,
        revoked_devices_have_no_owner_and_are_not_listed,
        inserting_a_known_device_id_changes_nothing,
        a_device_of_an_unknown_user_is_refused,
        revoking_an_unknown_device_does_nothing,
    ]
}
```

Create `crates/push-testkit/src/contract/contacts.rs`:

```rust
use push_domain::{ContactState, DomainError, InsertOutcome, Store, Tx, UserId};

use super::{add_users, invite, start};
use crate::{Backend, Case, case_list};

async fn contact_rows_are_per_direction_and_replaced_by_put<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (alice, bob, carol) = (UserId::new(), UserId::new(), UserId::new());
    add_users(&store, &[alice, bob, carol]).await;
    let mut tx = store.begin().await.unwrap();
    tx.lock_contact_pair(alice, bob).await.unwrap();
    tx.lock_contact_pair(bob, alice).await.unwrap();
    tx.put_contact(alice, bob, ContactState::RequestedOut)
        .await
        .unwrap();
    tx.put_contact(alice, bob, ContactState::Accepted)
        .await
        .unwrap();
    tx.put_contact(alice, carol, ContactState::Blocked)
        .await
        .unwrap();
    tx.put_contact(bob, alice, ContactState::Accepted)
        .await
        .unwrap();

    assert_eq!(
        tx.get_contact(alice, bob).await,
        Ok(Some(ContactState::Accepted))
    );
    assert_eq!(tx.get_contact(carol, alice).await, Ok(None));
    let mut rows = tx.contacts_of(alice).await.unwrap();
    rows.sort_by_key(|(peer, _)| *peer);
    let mut expected = vec![
        (bob, ContactState::Accepted),
        (carol, ContactState::Blocked),
    ];
    expected.sort_by_key(|(peer, _)| *peer);
    assert_eq!(rows, expected);
    assert_eq!(tx.contacts_of(UserId::new()).await, Ok(vec![]));
}

async fn deleting_a_contact_removes_one_direction<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (alice, bob) = (UserId::new(), UserId::new());
    add_users(&store, &[alice, bob]).await;
    let mut tx = store.begin().await.unwrap();
    tx.put_contact(alice, bob, ContactState::Accepted)
        .await
        .unwrap();
    tx.put_contact(bob, alice, ContactState::Accepted)
        .await
        .unwrap();

    tx.delete_contact(alice, bob).await.unwrap();
    tx.delete_contact(alice, bob).await.unwrap();
    assert_eq!(tx.get_contact(alice, bob).await, Ok(None));
    assert_eq!(
        tx.get_contact(bob, alice).await,
        Ok(Some(ContactState::Accepted))
    );
}

async fn a_contact_with_an_unknown_user_is_refused<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let alice = UserId::new();
    add_users(&store, &[alice]).await;
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.put_contact(alice, UserId::new(), ContactState::RequestedOut)
            .await,
        Err(DomainError::Store(_))
    ));
}

async fn an_invite_code_is_stored_once<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (first, second) = (UserId::new(), UserId::new());
    add_users(&store, &[first, second]).await;
    let mut tx = store.begin().await.unwrap();

    assert_eq!(
        tx.insert_invite(invite("ABCDEFGHJK", first)).await,
        Ok(InsertOutcome::Inserted)
    );
    assert_eq!(
        tx.insert_invite(invite("ABCDEFGHJK", second)).await,
        Ok(InsertOutcome::Duplicate)
    );
    assert_eq!(
        tx.lock_invite("ABCDEFGHJK").await,
        Ok(Some(invite("ABCDEFGHJK", first)))
    );
    assert_eq!(tx.lock_invite("0000000000").await, Ok(None));
}

async fn an_invite_of_an_unknown_user_is_refused<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_invite(invite("0000000000", UserId::new())).await,
        Err(DomainError::Store(_))
    ));
}

async fn update_invite_changes_only_uses_and_revocation<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let user = UserId::new();
    add_users(&store, &[user]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_invite(invite("ABCDEFGHJK", user)).await.unwrap();

    let mut changed = invite("ABCDEFGHJK", UserId::new());
    changed.uses = 1;
    changed.revoked_at = Some(start());
    changed.max_uses = 9;
    tx.update_invite(changed).await.unwrap();

    let stored = tx.lock_invite("ABCDEFGHJK").await.unwrap().unwrap();
    assert_eq!((stored.uses, stored.revoked_at), (1, Some(start())));
    assert_eq!((stored.user_id, stored.max_uses), (user, 1));
    assert!(matches!(
        tx.update_invite(invite("0000000000", user)).await,
        Err(DomainError::Store(_))
    ));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        contact_rows_are_per_direction_and_replaced_by_put,
        deleting_a_contact_removes_one_direction,
        a_contact_with_an_unknown_user_is_refused,
        an_invite_code_is_stored_once,
        an_invite_of_an_unknown_user_is_refused,
        update_invite_changes_only_uses_and_revocation,
    ]
}
```

Create `crates/push-testkit/src/contract/conversations.rs`:

```rust
use chrono::TimeDelta;
use push_domain::{
    Conversation, ConversationId, ConversationKind, DomainError, InsertOutcome, Member, Role,
    Store, Tx, UserId,
};

use super::{add_users, conversation, member, start, store_with};
use crate::{Backend, Case, case_list};

async fn a_conversation_is_read_back_as_stored<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut group = conversation();
    group.kind = ConversationKind::Group;
    group.title = Some("Trip".into());
    let mut direct = conversation();
    direct.direct_key = Some([9; 32]);
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(group.clone()).await.unwrap();
    tx.insert_conversation(direct.clone()).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.get_conversation(group.id).await, Ok(Some(group)));
    assert_eq!(
        tx.get_conversation(direct.id).await,
        Ok(Some(direct.clone()))
    );
    assert_eq!(tx.get_conversation(ConversationId::new()).await, Ok(None));
    assert_eq!(tx.find_direct([9; 32]).await, Ok(Some(direct.id)));
    assert_eq!(tx.find_direct([8; 32]).await, Ok(None));
}

async fn next_seq_counts_from_one_and_a_rollback_gives_the_number_back<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.next_seq(id, start()).await.unwrap(), 1);
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.next_seq(id, start()).await.unwrap(), 2);
    tx.rollback().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.next_seq(id, start()).await.unwrap(), 2);
}

async fn next_seq_moves_updated_at_and_a_rollback_restores_it<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (first, second) = (start() + TimeDelta::hours(1), start() + TimeDelta::hours(2));

    let mut tx = store.begin().await.unwrap();
    tx.next_seq(id, first).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    tx.next_seq(id, second).await.unwrap();
    tx.rollback().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    let stored = tx.get_conversation(id).await.unwrap().unwrap();
    assert_eq!((stored.last_seq, stored.updated_at), (1, first));
}

async fn a_second_direct_conversation_for_the_same_pair_is_a_duplicate<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut first = conversation();
    first.direct_key = Some([7; 32]);
    let mut second = conversation();
    second.direct_key = Some([7; 32]);
    let second_id = second.id;

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.insert_conversation(first.clone()).await.unwrap(),
        InsertOutcome::Inserted
    );
    assert_eq!(
        tx.insert_conversation(second).await.unwrap(),
        InsertOutcome::Duplicate
    );
    assert_eq!(tx.find_direct([7; 32]).await.unwrap(), Some(first.id));
    assert_eq!(tx.get_conversation(second_id).await.unwrap(), None);
}

async fn a_conversation_id_cannot_be_inserted_twice<B: Backend>(backend: &B) {
    let first = conversation();
    let mut second = conversation();
    second.id = first.id;
    second.direct_key = Some([3; 32]);
    let store = store_with(backend, first).await;

    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_conversation(second).await,
        Err(DomainError::Store(_))
    ));
    tx.rollback().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.find_direct([3; 32]).await.unwrap(), None);
}

async fn set_title_changes_only_the_title<B: Backend>(backend: &B) {
    let original = conversation();
    let id = original.id;
    let store = store_with(backend, original.clone()).await;
    let mut tx = store.begin().await.unwrap();

    tx.set_title(id, "Trip".into()).await.unwrap();
    let stored = tx.get_conversation(id).await.unwrap().unwrap();
    assert_eq!(
        stored,
        Conversation {
            title: Some("Trip".into()),
            ..original
        }
    );
    assert_eq!(
        tx.set_title(ConversationId::new(), "Trip".into()).await,
        Err(DomainError::ConversationNotFound)
    );
}

async fn operations_on_missing_rows_fail_as_documented<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.next_seq(ConversationId::new(), start()).await,
        Err(DomainError::ConversationNotFound)
    );
    assert_eq!(
        tx.raise_read_seq(ConversationId::new(), UserId::new(), 1)
            .await,
        Err(DomainError::NotMember)
    );
}

async fn inserting_a_member_again_replaces_the_row<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (alice, bob) = (UserId::new(), UserId::new());
    add_users(&store, &[alice, bob]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_member(member(id, alice)).await.unwrap();
    tx.insert_member(member(id, bob)).await.unwrap();

    let changed = Member {
        role: Role::Admin,
        visible_from_seq: 4,
        left_at_seq: Some(9),
        read_seq: 6,
        ..member(id, alice)
    };
    tx.insert_member(changed.clone()).await.unwrap();
    assert_eq!(tx.get_member(id, alice).await, Ok(Some(changed.clone())));
    assert_eq!(tx.get_member(id, UserId::new()).await, Ok(None));
    let mut members = tx.members(id).await.unwrap();
    members.sort_by_key(|member| member.user_id);
    let mut expected = vec![changed, member(id, bob)];
    expected.sort_by_key(|member| member.user_id);
    assert_eq!(members, expected);
    assert_eq!(tx.members(ConversationId::new()).await, Ok(vec![]));
}

async fn a_member_needs_a_known_user_and_conversation<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let alice = UserId::new();
    add_users(&store, &[alice]).await;

    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_member(member(id, UserId::new())).await,
        Err(DomainError::Store(_))
    ));
    tx.rollback().await.unwrap();
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_member(member(ConversationId::new(), alice)).await,
        Err(DomainError::Store(_))
    ));
}

async fn memberships_pair_each_membership_with_its_current_conversation<B: Backend>(backend: &B) {
    let (first, second, elsewhere) = (conversation(), conversation(), conversation());
    let store = store_with(backend, first.clone()).await;
    let (alice, bob) = (UserId::new(), UserId::new());
    add_users(&store, &[alice, bob]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(second.clone()).await.unwrap();
    tx.insert_conversation(elsewhere.clone()).await.unwrap();
    tx.insert_member(member(first.id, alice)).await.unwrap();
    let left = Member {
        left_at_seq: Some(1),
        ..member(second.id, alice)
    };
    tx.insert_member(left.clone()).await.unwrap();
    tx.insert_member(member(elsewhere.id, bob)).await.unwrap();
    tx.next_seq(first.id, start()).await.unwrap();

    let mut memberships = tx.memberships(alice).await.unwrap();
    memberships.sort_by_key(|(conversation, _)| conversation.id);
    let mut expected = vec![
        (
            Conversation {
                last_seq: 1,
                ..first.clone()
            },
            member(first.id, alice),
        ),
        (second, left),
    ];
    expected.sort_by_key(|(conversation, _)| conversation.id);
    assert_eq!(memberships, expected);
    assert_eq!(tx.memberships(UserId::new()).await, Ok(vec![]));
}

async fn the_read_position_only_moves_forward<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let alice = UserId::new();
    add_users(&store, &[alice]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_member(member(id, alice)).await.unwrap();

    assert_eq!(tx.raise_read_seq(id, alice, 3).await, Ok(true));
    assert_eq!(tx.raise_read_seq(id, alice, 3).await, Ok(false));
    assert_eq!(tx.raise_read_seq(id, alice, 2).await, Ok(false));
    assert_eq!(tx.get_member(id, alice).await.unwrap().unwrap().read_seq, 3);
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        a_conversation_is_read_back_as_stored,
        next_seq_counts_from_one_and_a_rollback_gives_the_number_back,
        next_seq_moves_updated_at_and_a_rollback_restores_it,
        a_second_direct_conversation_for_the_same_pair_is_a_duplicate,
        a_conversation_id_cannot_be_inserted_twice,
        set_title_changes_only_the_title,
        operations_on_missing_rows_fail_as_documented,
        inserting_a_member_again_replaces_the_row,
        a_member_needs_a_known_user_and_conversation,
        memberships_pair_each_membership_with_its_current_conversation,
        the_read_position_only_moves_forward,
    ]
}
```

Create `crates/push-testkit/src/contract/entries.rs`:

```rust
use chrono::TimeDelta;
use push_domain::{
    ClientMsgId, ContentType, ConversationId, DeviceId, DomainError, Entry, EntryKind,
    InsertOutcome, MessageId, Store, Tx, UserId,
};

use super::{conversation, entry, seqs, start, store_with};
use crate::{Backend, Case, case_list};

async fn a_reused_client_msg_id_is_a_duplicate<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let client_msg_id = ClientMsgId::new();
    let mut tx = store.begin().await.unwrap();

    let first = entry(id, 1, client_msg_id);
    assert_eq!(
        tx.insert_entry(first.clone()).await.unwrap(),
        InsertOutcome::Inserted
    );
    assert_eq!(
        tx.insert_entry(entry(id, 2, client_msg_id)).await.unwrap(),
        InsertOutcome::Duplicate
    );
    assert_eq!(
        tx.entry_by_client_id(id, client_msg_id).await,
        Ok(Some(first))
    );
    assert_eq!(
        tx.entry_by_client_id(id, ClientMsgId::new()).await,
        Ok(None)
    );
    assert_eq!(
        seqs(&tx.entries_after(id, 0, 5, 0, 10).await.unwrap()),
        vec![1]
    );
}

async fn every_field_of_an_entry_is_read_back<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let edit = Entry {
        kind: EntryKind::Edit,
        target_seq: Some(1),
        sender_device_id: Some(DeviceId::new()),
        content_type: ContentType::SystemEvent,
        body: None,
        client_ts: Some(start() - TimeDelta::seconds(3)),
        edited_at: Some(start() + TimeDelta::minutes(1)),
        edit_seq: Some(7),
        deleted_at: Some(start() + TimeDelta::minutes(2)),
        ..entry(id, 2, ClientMsgId::new())
    };
    let mut tx = store.begin().await.unwrap();
    tx.insert_entry(entry(id, 1, ClientMsgId::new()))
        .await
        .unwrap();
    tx.insert_entry(edit.clone()).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.entry_by_message_id(id, edit.message_id).await,
        Ok(Some(edit))
    );
}

async fn entry_ranges_respect_their_bounds<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();
    for seq in 1..=5 {
        tx.insert_entry(entry(id, seq, ClientMsgId::new()))
            .await
            .unwrap();
    }

    assert_eq!(
        seqs(&tx.entries_after(id, 1, 4, 0, 10).await.unwrap()),
        vec![2, 3, 4]
    );
    assert_eq!(
        seqs(&tx.entries_after(id, 1, 5, 0, 2).await.unwrap()),
        vec![2, 3]
    );
    assert_eq!(
        seqs(&tx.entries_after(id, 5, 5, 0, 10).await.unwrap()),
        Vec::<i64>::new()
    );
    assert_eq!(
        seqs(&tx.messages_before(id, 1, 5, 2).await.unwrap()),
        vec![3, 4]
    );
    assert_eq!(
        seqs(&tx.messages_before(id, 2, 3, 10).await.unwrap()),
        vec![2]
    );
    assert_eq!(
        seqs(&tx.messages_before(id, 1, 1, 10).await.unwrap()),
        Vec::<i64>::new()
    );
}

async fn only_live_messages_from_others_are_counted_up_to_the_cap<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let me = UserId::new();
    let mut tx = store.begin().await.unwrap();
    let mut entries = vec![
        entry(id, 1, ClientMsgId::new()),
        Entry {
            sender_id: me,
            ..entry(id, 2, ClientMsgId::new())
        },
        Entry {
            deleted_at: Some(start()),
            ..entry(id, 3, ClientMsgId::new())
        },
        Entry {
            kind: EntryKind::Edit,
            ..entry(id, 4, ClientMsgId::new())
        },
    ];
    entries.extend((5..=7).map(|seq| entry(id, seq, ClientMsgId::new())));
    for entry in entries {
        tx.insert_entry(entry).await.unwrap();
    }

    assert_eq!(tx.count_incoming_messages(id, me, 0, 7, 100).await, Ok(4));
    assert_eq!(tx.count_incoming_messages(id, me, 1, 4, 100).await, Ok(0));
    assert_eq!(tx.count_incoming_messages(id, me, 0, 7, 2).await, Ok(2));
    assert_eq!(tx.count_incoming_messages(id, me, 7, 7, 100).await, Ok(0));
}

async fn messages_before_skips_edits_and_deletes<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();
    let kinds = [
        EntryKind::Message,
        EntryKind::Edit,
        EntryKind::System,
        EntryKind::Delete,
        EntryKind::Message,
    ];
    for (seq, kind) in (1..).zip(kinds) {
        tx.insert_entry(Entry {
            kind,
            ..entry(id, seq, ClientMsgId::new())
        })
        .await
        .unwrap();
    }

    assert_eq!(
        seqs(&tx.messages_before(id, 1, 6, 2).await.unwrap()),
        vec![3, 5]
    );
    assert_eq!(
        seqs(&tx.messages_before(id, 1, 6, 10).await.unwrap()),
        vec![1, 3, 5]
    );
}

async fn an_entry_of_an_unknown_conversation_is_refused<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_entry(entry(ConversationId::new(), 1, ClientMsgId::new()))
            .await,
        Err(DomainError::Store(_))
    ));
}

async fn a_message_id_cannot_be_inserted_twice<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let first = entry(id, 1, ClientMsgId::new());
    let mut second = entry(id, 2, ClientMsgId::new());
    second.message_id = first.message_id;
    let mut tx = store.begin().await.unwrap();
    tx.insert_entry(first).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert!(matches!(
        tx.insert_entry(second).await,
        Err(DomainError::Store(_))
    ));
    tx.rollback().await.unwrap();
    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        seqs(&tx.entries_after(id, 0, 5, 0, 10).await.unwrap()),
        vec![1]
    );
}

async fn an_entry_cannot_overwrite_an_existing_seq<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_entry(entry(id, 1, ClientMsgId::new()))
        .await
        .unwrap();

    assert!(matches!(
        tx.insert_entry(entry(id, 1, ClientMsgId::new())).await,
        Err(DomainError::Store(_))
    ));
}

async fn an_entry_is_found_by_message_id_only_in_its_own_conversation<B: Backend>(backend: &B) {
    let (home, elsewhere) = (conversation(), conversation());
    let (home_id, elsewhere_id) = (home.id, elsewhere.id);
    let store = store_with(backend, home).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(elsewhere).await.unwrap();
    let stored = entry(home_id, 1, ClientMsgId::new());
    tx.insert_entry(stored.clone()).await.unwrap();

    assert_eq!(
        tx.entry_by_message_id(home_id, stored.message_id)
            .await
            .unwrap(),
        Some(stored.clone())
    );
    assert_eq!(
        tx.entry_by_message_id(elsewhere_id, stored.message_id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        tx.entry_by_message_id(home_id, MessageId::new())
            .await
            .unwrap(),
        None
    );
}

async fn update_entry_changes_only_the_mutable_fields<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();
    let original = entry(id, 1, ClientMsgId::new());
    tx.insert_entry(original.clone()).await.unwrap();

    let mut changed = original.clone();
    changed.body = None;
    changed.edited_at = Some(start() + TimeDelta::hours(1));
    changed.edit_seq = Some(4);
    changed.deleted_at = Some(start() + TimeDelta::hours(2));
    changed.sender_id = UserId::new();
    changed.kind = EntryKind::Edit;
    tx.update_entry(changed.clone()).await.unwrap();

    let stored = tx
        .entry_by_message_id(id, original.message_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.body, None);
    assert_eq!(stored.edited_at, changed.edited_at);
    assert_eq!(stored.edit_seq, Some(4));
    assert_eq!(stored.deleted_at, changed.deleted_at);
    assert_eq!(stored.sender_id, original.sender_id);
    assert_eq!(stored.kind, EntryKind::Message);
}

async fn updating_a_missing_entry_fails<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();

    assert!(matches!(
        tx.update_entry(entry(id, 1, ClientMsgId::new())).await,
        Err(DomainError::Store(_))
    ));
}

async fn erasing_edit_bodies_touches_only_edits_of_that_target<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();
    let edit = |seq, target| Entry {
        kind: EntryKind::Edit,
        target_seq: Some(target),
        ..entry(id, seq, ClientMsgId::new())
    };
    tx.insert_entry(entry(id, 1, ClientMsgId::new()))
        .await
        .unwrap();
    tx.insert_entry(entry(id, 2, ClientMsgId::new()))
        .await
        .unwrap();
    tx.insert_entry(edit(3, 1)).await.unwrap();
    tx.insert_entry(edit(4, 2)).await.unwrap();
    tx.insert_entry(edit(5, 1)).await.unwrap();

    tx.erase_edit_bodies(id, 1).await.unwrap();
    let bodies: Vec<bool> = tx
        .entries_after(id, 0, 5, 0, 10)
        .await
        .unwrap()
        .iter()
        .map(|entry| entry.body.is_some())
        .collect();
    assert_eq!(bodies, [true, true, false, true, false]);
}

async fn entries_after_skips_changes_to_messages_below_targets_from<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();
    for seq in 1..=3 {
        tx.insert_entry(entry(id, seq, ClientMsgId::new()))
            .await
            .unwrap();
    }
    for (seq, target) in [(4, 1), (5, 3), (6, 2)] {
        tx.insert_entry(Entry {
            kind: EntryKind::Edit,
            target_seq: Some(target),
            ..entry(id, seq, ClientMsgId::new())
        })
        .await
        .unwrap();
    }

    assert_eq!(
        seqs(&tx.entries_after(id, 2, 6, 3, 10).await.unwrap()),
        vec![3, 5]
    );
    assert_eq!(
        seqs(&tx.entries_after(id, 0, 6, 2, 3).await.unwrap()),
        vec![1, 2, 3],
        "the limit counts only returned entries"
    );
    assert_eq!(
        seqs(&tx.entries_after(id, 3, 6, 2, 2).await.unwrap()),
        vec![5, 6]
    );
}

async fn the_first_live_message_skips_changes_deleted_messages_and_the_excluded_sender<
    B: Backend,
>(
    backend: &B,
) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let me = UserId::new();
    let mut tx = store.begin().await.unwrap();
    let entries = [
        entry(id, 1, ClientMsgId::new()),
        Entry {
            sender_id: me,
            ..entry(id, 2, ClientMsgId::new())
        },
        Entry {
            deleted_at: Some(start()),
            ..entry(id, 3, ClientMsgId::new())
        },
        Entry {
            kind: EntryKind::Edit,
            ..entry(id, 4, ClientMsgId::new())
        },
        entry(id, 5, ClientMsgId::new()),
    ];
    for entry in entries {
        tx.insert_entry(entry).await.unwrap();
    }

    assert_eq!(tx.first_live_message(id, 0, 5, Some(me)).await, Ok(Some(1)));
    assert_eq!(tx.first_live_message(id, 1, 5, Some(me)).await, Ok(Some(5)));
    assert_eq!(tx.first_live_message(id, 1, 5, None).await, Ok(Some(2)));
    assert_eq!(tx.first_live_message(id, 2, 4, None).await, Ok(None));
    assert_eq!(tx.first_live_message(id, 5, 5, None).await, Ok(None));
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        a_reused_client_msg_id_is_a_duplicate,
        every_field_of_an_entry_is_read_back,
        entry_ranges_respect_their_bounds,
        only_live_messages_from_others_are_counted_up_to_the_cap,
        messages_before_skips_edits_and_deletes,
        an_entry_of_an_unknown_conversation_is_refused,
        a_message_id_cannot_be_inserted_twice,
        an_entry_cannot_overwrite_an_existing_seq,
        an_entry_is_found_by_message_id_only_in_its_own_conversation,
        update_entry_changes_only_the_mutable_fields,
        updating_a_missing_entry_fails,
        erasing_edit_bodies_touches_only_edits_of_that_target,
        entries_after_skips_changes_to_messages_below_targets_from,
        the_first_live_message_skips_changes_deleted_messages_and_the_excluded_sender,
    ]
}
```

Create `crates/push-testkit/src/contract/positions.rs`:

```rust
use push_domain::{ConversationId, DeviceId, DomainError, Store, Tx, UserId};

use super::{add_users, conversation, store_with};
use crate::{Backend, Case, case_list};

async fn positions_only_move_forward<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    add_users(&store, &[user]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(device, user).await.unwrap();

    assert_eq!(tx.get_cursor(device, id).await, Ok(None));
    tx.create_cursor_if_absent(device, id, 3).await.unwrap();
    tx.create_cursor_if_absent(device, id, 9).await.unwrap();
    assert_eq!(tx.get_cursor(device, id).await, Ok(Some(3)));

    assert!(tx.raise_cursor(device, id, 5).await.unwrap());
    assert!(!tx.raise_cursor(device, id, 5).await.unwrap());
    assert!(!tx.raise_cursor(device, id, 4).await.unwrap());
    assert_eq!(tx.get_cursor(device, id).await, Ok(Some(5)));
}

async fn raising_an_absent_position_creates_it<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    add_users(&store, &[user]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(device, user).await.unwrap();

    assert!(tx.raise_cursor(device, id, 4).await.unwrap());
    assert_eq!(tx.get_cursor(device, id).await.unwrap(), Some(4));
}

async fn delivered_is_the_furthest_position_of_any_device_of_the_user<B: Backend>(backend: &B) {
    let (conversation, other) = (conversation(), conversation());
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (alice, bob) = (UserId::new(), UserId::new());
    add_users(&store, &[alice, bob]).await;
    let (phone, laptop, bobs) = (DeviceId::new(), DeviceId::new(), DeviceId::new());
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(other.clone()).await.unwrap();
    for (device, user) in [(phone, alice), (laptop, alice), (bobs, bob)] {
        tx.insert_device(device, user).await.unwrap();
    }
    assert_eq!(tx.delivered_seq(id, alice).await, Ok(0));

    tx.raise_cursor(phone, id, 3).await.unwrap();
    tx.raise_cursor(laptop, id, 5).await.unwrap();
    tx.raise_cursor(bobs, id, 8).await.unwrap();
    tx.raise_cursor(phone, other.id, 9).await.unwrap();
    tx.revoke_device(laptop).await.unwrap();

    assert_eq!(
        tx.delivered_seq(id, alice).await,
        Ok(5),
        "a revoked device's position still counts"
    );
    assert_eq!(tx.delivered_seq(id, bob).await, Ok(8));
    assert_eq!(tx.delivered_seq(id, UserId::new()).await, Ok(0));
}

async fn a_position_needs_a_known_device_and_conversation<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (user, device) = (UserId::new(), DeviceId::new());
    add_users(&store, &[user]).await;
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(device, user).await.unwrap();
    tx.commit().await.unwrap();

    let attempts: [(DeviceId, ConversationId, bool); 4] = [
        (DeviceId::new(), id, false),
        (device, ConversationId::new(), false),
        (DeviceId::new(), id, true),
        (device, ConversationId::new(), true),
    ];
    for (device, conversation, raise) in attempts {
        let mut tx = store.begin().await.unwrap();
        let result = if raise {
            tx.raise_cursor(device, conversation, 1).await.map(|_| ())
        } else {
            tx.create_cursor_if_absent(device, conversation, 0).await
        };
        assert!(matches!(result, Err(DomainError::Store(_))));
    }
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![
        positions_only_move_forward,
        raising_an_absent_position_creates_it,
        delivered_is_the_furthest_position_of_any_device_of_the_user,
        a_position_needs_a_known_device_and_conversation,
    ]
}
```

- [ ] **Step 2: Run the contract before the cases**

In `crates/push-testkit/src/lib.rs`, replace `mod scenarios;` with:

```rust
mod contract;
mod scenarios;
```

and replace the `cases` function with:

```rust
/// Every case that any store must pass: the `Tx` contract, then the domain scenarios.
pub fn cases<B: Backend>() -> Vec<Case<B>> {
    let mut cases = contract::cases();
    cases.extend(scenarios::cases());
    cases
}
```

- [ ] **Step 3: Run the contract against the in-memory store**

```powershell
cargo test -p push-testkit
```

Expected: 242 tests, all `ok` (43 `contract::…`, 199 scenarios).

- [ ] **Step 4: Remove the old unit tests**

In `crates/push-domain/src/memory.rs`, delete the whole `#[cfg(test)] mod tests { … }` module at the end of the file (from `#[cfg(test)]` to the end). In `crates/push-domain/Cargo.toml`, delete the `[dev-dependencies]` section (its only entry was tokio, for those tests) and the blank line after it.

In `crates/push-domain/src/store.rs`, in the list of implementation rules at the top, add after the line `//! - enforce the schema's foreign and primary keys, failing with `DomainError::Store`;`:

```rust
//! - after any method fails, expect the caller to roll back: the transaction may be
//!   unusable (on Postgres every later statement fails), so no caller continues after an
//!   error;
```

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 264 tests pass (`push-domain`: 22 unit; `in_memory`: 242), no warnings, no diff, the 1.85 build succeeds.

```powershell
git add -A crates Cargo.lock
git commit -m "test: check the Tx contract against every store"
```

---

### Task 4: Race-free device registration and a monotonic `updated_at`

**Files:**
- Modify: `crates/push-testkit/src/contract/users.rs`
- Modify: `crates/push-testkit/src/contract/conversations.rs`
- Modify: `crates/push-domain/src/store.rs`
- Modify: `crates/push-domain/src/memory.rs`
- Modify: `crates/push-domain/src/service.rs`

Decisions 3 and 4.

- [ ] **Step 1: Write the failing tests**

In `crates/push-testkit/src/contract/users.rs`, replace the case `inserting_a_known_device_id_changes_nothing` with:

```rust
async fn inserting_a_known_device_id_changes_nothing<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (user, other) = (UserId::new(), UserId::new());
    add_users(&store, &[user, other]).await;
    let (device, revoked) = (DeviceId::new(), DeviceId::new());
    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.insert_device(device, user).await, Ok(true));
    assert_eq!(tx.insert_device(device, user).await, Ok(true));
    assert_eq!(tx.insert_device(device, other).await, Ok(false));
    assert_eq!(tx.insert_device(revoked, user).await, Ok(true));
    tx.revoke_device(revoked).await.unwrap();
    assert_eq!(tx.insert_device(revoked, user).await, Ok(false));

    assert_eq!(tx.device_owner(device).await.unwrap(), Some(user));
    assert_eq!(tx.device_owner(revoked).await.unwrap(), None);
    assert!(tx.devices_of_user(other).await.unwrap().is_empty());
}
```

In `crates/push-testkit/src/contract/conversations.rs`, add after the case `next_seq_moves_updated_at_and_a_rollback_restores_it`:

```rust
async fn next_seq_never_moves_updated_at_back<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let later = start() + TimeDelta::hours(1);

    let mut tx = store.begin().await.unwrap();
    tx.next_seq(id, later).await.unwrap();
    tx.next_seq(id, start()).await.unwrap();

    let stored = tx.get_conversation(id).await.unwrap().unwrap();
    assert_eq!((stored.last_seq, stored.updated_at), (2, later));
}
```

and add `next_seq_never_moves_updated_at_back,` to its case list, after `next_seq_moves_updated_at_and_a_rollback_restores_it,`.

- [ ] **Step 2: Run them to watch them fail**

```powershell
cargo test -p push-testkit
```

Expected: a compile error, `mismatched types … expected `()`, found `bool`` in `users.rs` (`insert_device` returns nothing yet).

- [ ] **Step 3: Change the contract**

In `crates/push-domain/src/store.rs`, replace the doc comment and declaration of `insert_device`, and delete `device_exists` with its doc comment, so that this part reads:

```rust
    /// Records an active device of `user` unless a device with this id exists (any owner,
    /// active or revoked), and returns whether the device is now an active device of
    /// `user`. Postgres checks after `INSERT … ON CONFLICT DO NOTHING`, which waits for a
    /// concurrent insert of the same id, so two registrations cannot both see `true` for
    /// different users. Fails with `DomainError::Store` if the device is new and the user
    /// is not known.
    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool>;
    /// Marks a device revoked. A no-op for an unknown device.
    async fn revoke_device(&mut self, device: DeviceId) -> DomainResult<()>;
    /// The owner of an active device; `None` if the device is unknown or revoked.
    async fn device_owner(&mut self, device: DeviceId) -> DomainResult<Option<UserId>>;
```

and replace the first two doc lines of `next_seq` with:

```rust
    /// Locks the conversation, increments `last_seq`, moves `updated_at` forward to `at`
    /// (never back: `GREATEST` on Postgres) and returns the new `last_seq`. Both changes are
    /// undone if the transaction rolls back.
```

- [ ] **Step 4: Implement it in memory**

In `crates/push-domain/src/memory.rs`, replace `insert_device` with:

```rust
    async fn insert_device(&mut self, device: DeviceId, user: UserId) -> DomainResult<bool> {
        if let Some(row) = self.work.devices.get(&device) {
            return Ok(row.user == user && !row.revoked);
        }
        if !self.work.users.contains(&user) {
            return Err(missing("user"));
        }
        self.work.devices.insert(
            device,
            DeviceRow {
                user,
                revoked: false,
            },
        );
        Ok(true)
    }
```

delete the method `device_exists`, and in `next_seq` replace `conversation.updated_at = at;` with:

```rust
        conversation.updated_at = conversation.updated_at.max(at);
```

- [ ] **Step 5: Register devices through it**

In `crates/push-domain/src/service.rs`, in `register_device`, replace

```rust
        if tx.device_exists(device).await? && tx.device_owner(device).await? != Some(user) {
            return Err(DomainError::UnknownDevice);
        }
        tx.insert_device(device, user).await?;
```

with:

```rust
        if !tx.insert_device(device, user).await? {
            return Err(DomainError::UnknownDevice);
        }
```

- [ ] **Step 6: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 265 tests pass (22 unit, 243 `in_memory`), no warnings, no diff, the 1.85 build succeeds. The `devices::…` scenarios (another user's device id, a revoked device) still pass through the new path.

```powershell
git add -A crates
git commit -m "feat(domain): register devices race-free and keep updated_at monotonic"
```

---

### Task 5: Serialise group position changes per conversation

**Files:**
- Modify: `crates/push-testkit/src/contract/conversations.rs`
- Modify: `crates/push-domain/src/store.rs`
- Modify: `crates/push-domain/src/memory.rs`
- Modify: `crates/push-domain/src/read.rs`, `sync.rs`, `send.rs`, `groups.rs`

Decision 5. The in-memory store serialises everything already, so no in-memory test can
see the race; Plan 3B's `concurrency::concurrent_acks_by_different_members_produce_the_group_receipt`
fails on Postgres without this lock. Here the contract case pins what every store must
allow: taking the lock twice, and for a conversation that does not exist.

- [ ] **Step 1: Write the failing contract case**

In `crates/push-testkit/src/contract/conversations.rs`, add after the case `the_read_position_only_moves_forward`:

```rust
async fn the_positions_lock_can_be_taken_again_and_needs_no_row<B: Backend>(backend: &B) {
    let conversation = conversation();
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let mut tx = store.begin().await.unwrap();

    assert_eq!(tx.lock_conversation_positions(id).await, Ok(()));
    assert_eq!(tx.lock_conversation_positions(id).await, Ok(()));
    assert_eq!(
        tx.lock_conversation_positions(ConversationId::new()).await,
        Ok(())
    );
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.lock_conversation_positions(id).await, Ok(()));
}
```

and add `the_positions_lock_can_be_taken_again_and_needs_no_row,` at the end of its case list.

- [ ] **Step 2: Run it to watch it fail**

```powershell
cargo test -p push-testkit
```

Expected: a compile error, `no method named `lock_conversation_positions` found`.

- [ ] **Step 3: Add the lock to the contract**

In `crates/push-domain/src/store.rs`, add after the declaration of `raise_read_seq`:

```rust
    /// Serialises changes to the conversation's delivery and read positions until the
    /// transaction ends. Every operation that compares a group's group-wide positions
    /// before and after a change calls it before reading "before", so two concurrent
    /// changes cannot both miss a move (under READ COMMITTED each would otherwise see the
    /// other's old position). It does not lock the conversation row: acks and reads do not
    /// wait for appends to commit. Appends take it after `next_seq`, never before.
    /// Postgres: `pg_advisory_xact_lock` on a hash of the conversation id.
    async fn lock_conversation_positions(
        &mut self,
        conversation: ConversationId,
    ) -> DomainResult<()>;
```

and in the `# Locking` section of the module documentation, add after the paragraph about redeeming an invite:

```rust
//!
//! A group's positions lock ([`Tx::lock_conversation_positions`]) comes after its
//! conversation lock: appends and group changes take the conversation lock through
//! [`Tx::next_seq`] first, acks and reads take only the positions lock.
```

In `crates/push-domain/src/memory.rs`, add after `raise_read_seq`:

```rust
    async fn lock_conversation_positions(
        &mut self,
        _conversation: ConversationId,
    ) -> DomainResult<()> {
        // Every in-memory transaction already holds the store-wide lock.
        Ok(())
    }
```

- [ ] **Step 4: Take it before every "before"**

In `crates/push-domain/src/read.rs`, add before the function `group_positions_for`:

```rust
/// The group-wide positions before a change to `conversation`, with its log ending at
/// `last_seq`, after taking its positions lock so that concurrent changes are compared
/// one after the other. `None`, without locking, for a direct conversation.
pub(crate) async fn group_positions_before<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    last_seq: i64,
) -> DomainResult<Option<GroupPositions>> {
    if conversation.kind != ConversationKind::Group {
        return Ok(None);
    }
    tx.lock_conversation_positions(conversation.id).await?;
    group_positions(tx, conversation, last_seq).await
}
```

Then make the four callers that compute "before" use it:

- `read.rs`, in `read_up_to`: `let group_before = group_positions(&mut tx, &found, found.last_seq).await?;`
  becomes `let group_before = group_positions_before(&mut tx, &found, found.last_seq).await?;`.
- `sync.rs`, in `ack`: the same replacement; and the import
  `use crate::read::{direct_receipt, group_positions, group_receipt};` becomes
  `use crate::read::{direct_receipt, group_positions_before, group_receipt};`.
- `send.rs`, in `prepare_append`: `let group_before = group_positions(&mut tx, &conversation, seq - 1).await?;`
  becomes `let group_before = group_positions_before(&mut tx, &conversation, seq - 1).await?;`;
  and the import `use crate::read::{GroupPositions, direct_receipt, group_positions, group_receipt};`
  becomes `use crate::read::{GroupPositions, direct_receipt, group_positions_before, group_receipt};`.
- `groups.rs`, in `begin_change`: `let before = group_positions(&mut tx, &conversation, seq - 1).await?;`
  becomes `let before = group_positions_before(&mut tx, &conversation, seq - 1).await?;`;
  and the import `use crate::read::{GroupPositions, group_positions, group_receipt};` becomes
  `use crate::read::{GroupPositions, group_positions_before, group_receipt};`.

In `prepare_append` and `begin_change` the call already comes after `next_seq`, which is the lock order the contract documents.

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 266 tests pass (22 unit, 244 `in_memory`), no warnings, no diff, the 1.85 build succeeds.

```powershell
git add -A crates
git commit -m "feat(domain): serialise group position changes per conversation"
```

---

### Task 6: Group-wide positions in one store call

**Files:**
- Modify: `crates/push-testkit/src/contract/positions.rs`
- Modify: `crates/push-domain/src/model.rs`, `lib.rs`, `store.rs`, `memory.rs`, `read.rs`, `send.rs`, `groups.rs`

Decision 6. Today `group_positions` costs up to three store calls per current member, twice
per change: about 600 statements for a 100-member group. The rule moves into
`Tx::group_positions`; the in-memory store keeps the old loop as the reference.

- [ ] **Step 1: Write the failing contract cases**

In `crates/push-testkit/src/contract/positions.rs`, replace the imports with:

```rust
use push_domain::{
    ClientMsgId, Conversation, ConversationId, ConversationKind, DeviceId, DomainError, Entry,
    EntryKind, GroupPositions, Member, Store, Tx, UserId,
};

use super::{add_users, conversation, entry, member, start, store_with};
```

add before the case list:

```rust
/// A group log with seven entries and five members:
///
/// | seq | entry |
/// |---|---|
/// | 1 | system entry by alice |
/// | 2 | message from alice |
/// | 3 | message from bob |
/// | 4 | message from carol, deleted |
/// | 5 | edit by alice of seq 2 |
/// | 6 | message from alice |
/// | 7 | message from bob |
///
/// alice has read 2 and her device has 7; bob has read 0, his revoked phone has 6 and his
/// laptop 1; carol has read 5 and has no device; dave left at 3; erin joined at 6 and her
/// device still has 3 from an earlier membership.
async fn group_positions_are_minimums_of_effective_positions_over_current_members<B: Backend>(
    backend: &B,
) {
    let group = Conversation {
        kind: ConversationKind::Group,
        ..conversation()
    };
    let id = group.id;
    let store = store_with(backend, group).await;
    let [alice, bob, carol, dave, erin] = [(); 5].map(|()| UserId::new());
    add_users(&store, &[alice, bob, carol, dave, erin]).await;
    let mut tx = store.begin().await.unwrap();
    let senders = [alice, alice, bob, carol, alice, alice, bob];
    for (seq, sender) in (1..).zip(senders) {
        let mut entry = Entry {
            sender_id: sender,
            ..entry(id, seq, ClientMsgId::new())
        };
        match seq {
            1 => entry.kind = EntryKind::System,
            4 => entry.deleted_at = Some(start()),
            5 => {
                entry.kind = EntryKind::Edit;
                entry.target_seq = Some(2);
            }
            _ => {}
        }
        tx.insert_entry(entry).await.unwrap();
    }
    let rows = [
        (alice, 1, None, 2),
        (bob, 1, None, 0),
        (carol, 1, None, 5),
        (dave, 1, Some(3), 0),
        (erin, 6, None, 0),
    ];
    for (user, visible_from_seq, left_at_seq, read_seq) in rows {
        tx.insert_member(Member {
            visible_from_seq,
            left_at_seq,
            read_seq,
            ..member(id, user)
        })
        .await
        .unwrap();
    }
    let devices = [(alice, 7), (bob, 6), (bob, 1), (erin, 3)];
    let mut ids = Vec::new();
    for (user, position) in devices {
        let device = DeviceId::new();
        tx.insert_device(device, user).await.unwrap();
        tx.raise_cursor(device, id, position).await.unwrap();
        ids.push(device);
    }
    tx.revoke_device(ids[1]).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.group_positions(id, 7).await,
        Ok(Some(GroupPositions {
            delivered: 5,
            read: 1
        }))
    );
    assert_eq!(
        tx.group_positions(id, 4).await,
        Ok(Some(GroupPositions {
            delivered: 4,
            read: 1
        })),
        "positions are capped at the end of the log"
    );
}

async fn a_group_without_current_members_has_no_positions<B: Backend>(backend: &B) {
    let group = Conversation {
        kind: ConversationKind::Group,
        ..conversation()
    };
    let id = group.id;
    let store = store_with(backend, group).await;
    let alice = UserId::new();
    add_users(&store, &[alice]).await;
    let mut tx = store.begin().await.unwrap();
    assert_eq!(tx.group_positions(id, 0).await, Ok(None));

    tx.insert_member(Member {
        left_at_seq: Some(0),
        ..member(id, alice)
    })
    .await
    .unwrap();
    assert_eq!(tx.group_positions(id, 0).await, Ok(None));
}
```

and add the two new cases at the end of the case list:

```rust
        group_positions_are_minimums_of_effective_positions_over_current_members,
        a_group_without_current_members_has_no_positions,
```

- [ ] **Step 2: Run them to watch them fail**

```powershell
cargo test -p push-testkit
```

Expected: compile errors, `unresolved import `push_domain::GroupPositions`` and `no method named `group_positions` found`.

- [ ] **Step 3: Make `GroupPositions` a public model type**

In `crates/push-domain/src/model.rs`, add before `pub struct ConversationState`'s `#[derive(…)]` line:

```rust
/// Group-wide positions (spec §4.5): every live message up to `read` has been read, and
/// every one up to `delivered` delivered, by every current member other than its sender.
/// `delivered` is never below `read`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupPositions {
    pub delivered: i64,
    pub read: i64,
}
```

In `crates/push-domain/src/lib.rs`, replace the `pub use model::{…};` statement with:

```rust
pub use model::{
    ContactState, ContactSummary, ContentType, Conversation, ConversationKind, ConversationState,
    ConversationSummary, Entry, EntryKind, GroupPositions, Invite, InviteRecord, Member,
    MemberState, Positions, Receipts, Role, SendReceipt, direct_key,
};
```

- [ ] **Step 4: Add `group_positions` to the contract**

In `crates/push-domain/src/store.rs`, replace `use crate::model::{…};` with:

```rust
use crate::model::{ContactState, Conversation, Entry, GroupPositions, InviteRecord, Member};
```

and add after the declaration of `delivered_seq` (the last method of the trait):

```rust
    /// The group-wide positions of `conversation` with its log ending at `last_seq`: the
    /// minimums over its current members of their effective positions. `None` if it has
    /// no current members.
    ///
    /// A member's effective position from a raw position `p` skips their own messages,
    /// edits, deletes, system entries and deleted messages: with
    /// `from = max(p, visible_from_seq - 1)`, it is `last_seq` if `from >= last_seq`,
    /// otherwise the `seq` before the first live message from someone else with
    /// `from < seq <= last_seq` (see `first_live_message`), or `last_seq` if there is none.
    /// Their effective read position starts from `read_seq`; their effective delivered
    /// position from `max(delivered_seq, read_seq)`, so it is never below the read one.
    ///
    /// Postgres computes it in one statement, however many members the group has.
    async fn group_positions(
        &mut self,
        conversation: ConversationId,
        last_seq: i64,
    ) -> DomainResult<Option<GroupPositions>>;
```

- [ ] **Step 5: Implement it in memory**

In `crates/push-domain/src/memory.rs`, replace `use crate::model::{…};` with:

```rust
use crate::model::{
    ContactState, Conversation, Entry, EntryKind, GroupPositions, InviteRecord, Member,
};
```

add this `impl` block between `struct State` and `struct DeviceRow`:

```rust
impl State {
    fn first_live_message(
        &self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        not_from: Option<UserId>,
    ) -> Option<i64> {
        if after >= up_to {
            return None;
        }
        self.entries
            .range((conversation, after + 1)..=(conversation, up_to))
            .map(|(_, entry)| entry)
            .find(|entry| {
                entry.kind == EntryKind::Message
                    && entry.deleted_at.is_none()
                    && not_from.is_none_or(|user| entry.sender_id != user)
            })
            .map(|entry| entry.seq)
    }

    fn delivered_seq(&self, conversation: ConversationId, user: UserId) -> i64 {
        self.cursors
            .iter()
            .filter(|((device, cursor_conversation), _)| {
                *cursor_conversation == conversation
                    && self.devices.get(device).is_some_and(|row| row.user == user)
            })
            .map(|(_, position)| *position)
            .max()
            .unwrap_or(0)
    }

    /// See [`Tx::group_positions`].
    fn effective_position(&self, member: &Member, last_seq: i64, position: i64) -> i64 {
        let upper = member.visible_upper(last_seq);
        let from = position.max(member.visible_from_seq - 1);
        if from >= upper {
            return upper;
        }
        self.first_live_message(member.conversation_id, from, upper, Some(member.user_id))
            .map_or(upper, |seq| seq - 1)
    }
}
```

replace the bodies of `first_live_message` and `delivered_seq` in `impl Tx for InMemoryTx` so they call the helpers:

```rust
    async fn first_live_message(
        &mut self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        not_from: Option<UserId>,
    ) -> DomainResult<Option<i64>> {
        Ok(self
            .work
            .first_live_message(conversation, after, up_to, not_from))
    }

    async fn delivered_seq(
        &mut self,
        conversation: ConversationId,
        user: UserId,
    ) -> DomainResult<i64> {
        Ok(self.work.delivered_seq(conversation, user))
    }
```

and add after `delivered_seq`, at the end of the `impl Tx for InMemoryTx` block:

```rust
    async fn group_positions(
        &mut self,
        conversation: ConversationId,
        last_seq: i64,
    ) -> DomainResult<Option<GroupPositions>> {
        let state = &self.work;
        Ok(state
            .members
            .values()
            .filter(|member| member.conversation_id == conversation && member.is_current())
            .map(|member| {
                let delivered_seq = state.delivered_seq(conversation, member.user_id);
                GroupPositions {
                    delivered: state.effective_position(
                        member,
                        last_seq,
                        delivered_seq.max(member.read_seq),
                    ),
                    read: state.effective_position(member, last_seq, member.read_seq),
                }
            })
            .reduce(|all, one| GroupPositions {
                delivered: all.delivered.min(one.delivered),
                read: all.read.min(one.read),
            }))
    }
```

- [ ] **Step 6: Use it in the domain**

In `crates/push-domain/src/read.rs`, replace the `use crate::model::{…};` statement with:

```rust
use crate::model::{
    Conversation, ConversationKind, ConversationState, GroupPositions, Member, MemberState,
    Positions,
};
```

delete the struct `GroupPositions` with its doc comment and derive, delete the function `effective_position` with its doc comment, and replace the function `group_positions` (with its doc comment) with:

```rust
/// The group-wide positions of `conversation` with its log ending at `last_seq` (see
/// [`Tx::group_positions`]). `None` for a direct conversation or a group without current
/// members.
pub(crate) async fn group_positions<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    last_seq: i64,
) -> DomainResult<Option<GroupPositions>> {
    if conversation.kind != ConversationKind::Group {
        return Ok(None);
    }
    tx.group_positions(conversation.id, last_seq).await
}
```

`GroupPositions` now comes from the model. In `crates/push-domain/src/send.rs`, replace the `use crate::model::{…};` and `use crate::read::{…};` statements with:

```rust
use crate::model::{
    ContentType, Conversation, ConversationKind, Entry, EntryKind, GroupPositions, Member,
    SendReceipt,
};
use crate::notify::{Event, Notifier};
use crate::read::{direct_receipt, group_positions_before, group_receipt};
```

In `crates/push-domain/src/groups.rs`, replace the `use crate::model::{…};`, `use crate::notify::{…};` and `use crate::read::{…};` statements with:

```rust
use crate::model::{
    ContentType, Conversation, ConversationKind, Entry, EntryKind, GroupPositions, Member, Role,
};
use crate::notify::{Event, Notifier};
use crate::read::{group_positions_before, group_receipt};
```

- [ ] **Step 7: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 268 tests pass (22 unit, 246 `in_memory`), no warnings, no diff, the 1.85 build succeeds. Every `group_receipts::…`, `listings::…` and `read_state::…` scenario passes unchanged.

```powershell
git add -A crates
git commit -m "feat(domain): compute group positions in one store call"
```

---

### Task 7: Members with their delivery positions in one store call

**Files:**
- Modify: `crates/push-testkit/src/contract/positions.rs`
- Modify: `crates/push-domain/src/store.rs`, `memory.rs`, `read.rs`

Decision 6. `conversation_state` of a direct conversation and every direct receipt read the
members and then each member's delivery position.

- [ ] **Step 1: Write the failing contract case**

In `crates/push-testkit/src/contract/positions.rs`, add before the doc comment `/// A group log with seven entries and five members:`:

```rust
async fn member_positions_pair_every_member_with_their_furthest_delivery<B: Backend>(backend: &B) {
    let (conversation, other) = (conversation(), conversation());
    let id = conversation.id;
    let store = store_with(backend, conversation).await;
    let (alice, bob, carol) = (UserId::new(), UserId::new(), UserId::new());
    add_users(&store, &[alice, bob, carol]).await;
    let (phone, laptop, bobs) = (DeviceId::new(), DeviceId::new(), DeviceId::new());
    let mut tx = store.begin().await.unwrap();
    tx.insert_conversation(other.clone()).await.unwrap();
    let former = Member {
        left_at_seq: Some(2),
        read_seq: 2,
        ..member(id, carol)
    };
    for row in [member(id, alice), member(id, bob), former.clone()] {
        tx.insert_member(row).await.unwrap();
    }
    tx.insert_member(member(other.id, alice)).await.unwrap();
    for (device, user) in [(phone, alice), (laptop, alice), (bobs, bob)] {
        tx.insert_device(device, user).await.unwrap();
    }
    tx.raise_cursor(phone, id, 3).await.unwrap();
    tx.raise_cursor(laptop, id, 4).await.unwrap();
    tx.raise_cursor(bobs, other.id, 9).await.unwrap();
    tx.revoke_device(laptop).await.unwrap();

    let mut positions = tx.member_positions(id).await.unwrap();
    positions.sort_by_key(|(member, _)| member.user_id);
    let mut expected = vec![(member(id, alice), 4), (member(id, bob), 0), (former, 0)];
    expected.sort_by_key(|(member, _)| member.user_id);
    assert_eq!(positions, expected);
    assert_eq!(tx.member_positions(ConversationId::new()).await, Ok(vec![]));
}
```

and add `member_positions_pair_every_member_with_their_furthest_delivery,` to the case list after `delivered_is_the_furthest_position_of_any_device_of_the_user,`.

- [ ] **Step 2: Run it to watch it fail**

```powershell
cargo test -p push-testkit
```

Expected: a compile error, `no method named `member_positions` found`.

- [ ] **Step 3: Add it to the contract and the in-memory store**

In `crates/push-domain/src/store.rs`, add after the declaration of `delivered_seq`:

```rust
    /// Every member row of the conversation, current and former, each with that member's
    /// `delivered_seq`. One statement on Postgres. The order of the result is unspecified.
    async fn member_positions(
        &mut self,
        conversation: ConversationId,
    ) -> DomainResult<Vec<(Member, i64)>>;
```

In `crates/push-domain/src/memory.rs`, add after `delivered_seq`:

```rust
    async fn member_positions(
        &mut self,
        conversation: ConversationId,
    ) -> DomainResult<Vec<(Member, i64)>> {
        let state = &self.work;
        Ok(state
            .members
            .values()
            .filter(|member| member.conversation_id == conversation)
            .map(|member| {
                let delivered_seq = state.delivered_seq(conversation, member.user_id);
                (member.clone(), delivered_seq)
            })
            .collect())
    }
```

- [ ] **Step 4: Use it in the domain**

In `crates/push-domain/src/read.rs`, in `conversation_state`, replace the `else` branch that builds `Positions::Members` with:

```rust
        } else {
            let members = tx
                .member_positions(conversation)
                .await?
                .into_iter()
                .map(|(member, delivered_seq)| MemberState {
                    user_id: member.user_id,
                    delivered_seq: delivered_seq.max(member.read_seq),
                    read_seq: member.read_seq,
                })
                .collect();
            Positions::Members(members)
        };
```

and in `direct_receipt`, replace everything from `let members = tx.members(conversation.id).await?;` through `let delivered_seq = tx.delivered_seq(conversation.id, user).await?;` with:

```rust
    let positions = tx.member_positions(conversation.id).await?;
    let (read_seq, delivered_seq) = positions
        .iter()
        .find(|(member, _)| member.user_id == user)
        .map(|(member, delivered_seq)| (member.read_seq, *delivered_seq))
        .ok_or(DomainError::NotMember)?;
    let members: Vec<Member> = positions.into_iter().map(|(member, _)| member).collect();
    let to: Vec<UserId> = current_member_ids(&members)
        .into_iter()
        .filter(|member| *member != user)
        .collect();
    if to.is_empty() {
        return Ok(None);
    }
```

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 269 tests pass (22 unit, 247 `in_memory`), no warnings, no diff, the 1.85 build succeeds.

```powershell
git add -A crates
git commit -m "feat(domain): read members with their delivery positions in one store call"
```

---

### Task 8: What a device has left to sync, in one store call

**Files:**
- Modify: `crates/push-testkit/src/contract/positions.rs`
- Modify: `crates/push-domain/src/store.rs`, `memory.rs`, `sync.rs`

Decision 6. `sync` reads every membership and then a delivery position per membership, on
every wake-up of every connected device.

- [ ] **Step 1: Write the failing contract case**

In `crates/push-testkit/src/contract/positions.rs`, add before the doc comment `/// A group log with seven entries and five members:`:

```rust
/// Six conversations of alice: the last `seq`, her member row (visible from, left at), her
/// phone's position if it has one, and the position sync should start from, if anything is
/// left to receive. Her laptop has everything; it does not count.
async fn sync_positions_list_memberships_with_something_left_to_receive<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let (alice, bob) = (UserId::new(), UserId::new());
    add_users(&store, &[alice, bob]).await;
    let (phone, laptop) = (DeviceId::new(), DeviceId::new());
    type Row = (i64, i64, Option<i64>, Option<i64>, Option<i64>);
    let rows: [Row; 6] = [
        (5, 1, None, Some(2), Some(2)),    // behind
        (4, 3, None, None, Some(2)),       // no position: starts at the floor
        (5, 1, None, Some(5), None),       // up to date
        (6, 1, Some(3), Some(3), None),    // left, has everything up to leaving
        (6, 1, Some(4), Some(1), Some(1)), // left, behind
        (4, 4, None, Some(1), Some(3)),    // position below the floor
    ];
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(phone, alice).await.unwrap();
    tx.insert_device(laptop, alice).await.unwrap();
    let mut expected = Vec::new();
    for (last_seq, visible_from_seq, left_at_seq, position, starts_at) in rows {
        let conversation = conversation();
        let id = conversation.id;
        tx.insert_conversation(conversation.clone()).await.unwrap();
        for _ in 0..last_seq {
            tx.next_seq(id, start()).await.unwrap();
        }
        let row = Member {
            visible_from_seq,
            left_at_seq,
            ..member(id, alice)
        };
        tx.insert_member(row.clone()).await.unwrap();
        tx.insert_member(member(id, bob)).await.unwrap();
        if let Some(position) = position {
            tx.raise_cursor(phone, id, position).await.unwrap();
        }
        tx.raise_cursor(laptop, id, last_seq).await.unwrap();
        if let Some(starts_at) = starts_at {
            let stored = Conversation {
                last_seq,
                ..conversation
            };
            expected.push((stored, row, starts_at));
        }
    }
    tx.commit().await.unwrap();

    let mut tx = store.begin().await.unwrap();
    let mut positions = tx.sync_positions(alice, phone).await.unwrap();
    positions.sort_by_key(|(conversation, _, _)| conversation.id);
    expected.sort_by_key(|(conversation, _, _)| conversation.id);
    assert_eq!(positions, expected);
    assert_eq!(tx.sync_positions(alice, laptop).await, Ok(vec![]));
}
```

and add `sync_positions_list_memberships_with_something_left_to_receive,` to the case list after `member_positions_pair_every_member_with_their_furthest_delivery,`.

- [ ] **Step 2: Run it to watch it fail**

```powershell
cargo test -p push-testkit
```

Expected: a compile error, `no method named `sync_positions` found`.

- [ ] **Step 3: Add it to the contract and the in-memory store**

In `crates/push-domain/src/store.rs`, add after the declaration of `member_positions`:

```rust
    /// The memberships of `user` in which `device` has entries left to receive, each with
    /// the position to sync from: the device's delivery position raised to the member's
    /// floor (`visible_from_seq - 1`), also when the device has no position. Only rows
    /// with that position below `member.visible_upper(conversation.last_seq)` are
    /// returned. As in `memberships`, each conversation is read in the same statement as
    /// its member row. The order of the result is unspecified.
    async fn sync_positions(
        &mut self,
        user: UserId,
        device: DeviceId,
    ) -> DomainResult<Vec<(Conversation, Member, i64)>>;
```

and in the `# Delivery positions` section of the module documentation, replace the lines from `//! change reads the memberships before the new member row commits.` to the end of the section with:

```rust
//! change reads the memberships before the new member row commits. Readers fall back to
//! `visible_from_seq - 1`, so [`Tx::sync_positions`] starts from the memberships and
//! joins the positions to them, never the other way round.
```

In `crates/push-domain/src/memory.rs`, add after `member_positions`:

```rust
    async fn sync_positions(
        &mut self,
        user: UserId,
        device: DeviceId,
    ) -> DomainResult<Vec<(Conversation, Member, i64)>> {
        let state = &self.work;
        Ok(state
            .members
            .values()
            .filter(|member| member.user_id == user)
            .filter_map(|member| {
                let conversation = state.conversations.get(&member.conversation_id)?;
                let floor = member.visible_from_seq - 1;
                let position = state
                    .cursors
                    .get(&(device, conversation.id))
                    .map_or(floor, |position| (*position).max(floor));
                (position < member.visible_upper(conversation.last_seq))
                    .then(|| (conversation.clone(), member.clone(), position))
            })
            .collect())
    }
```

- [ ] **Step 4: Sync through it**

In `crates/push-domain/src/sync.rs`, in `sync`, replace everything from `let mut memberships = tx.memberships(user).await?;` through the `if position >= upper { continue; }` block with:

```rust
        let mut pending = tx.sync_positions(user, device).await?;
        pending.sort_by_key(|(conversation, _, _)| conversation.id);

        let mut entries = Vec::new();
        let mut more = false;
        for (conversation, member, position) in pending {
            let upper = member.visible_upper(conversation.last_seq);
```

- [ ] **Step 5: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 270 tests pass (22 unit, 248 `in_memory`), no warnings, no diff, the 1.85 build succeeds. Every `sync::…` and `visibility::…` scenario passes unchanged.

```powershell
git add -A crates
git commit -m "feat(domain): find what a device has left to sync in one store call"
```

---

### Task 9: Conversation listings in two store calls

**Files:**
- Create: `crates/push-testkit/src/contract/overviews.rs`
- Modify: `crates/push-testkit/src/contract/mod.rs`
- Modify: `crates/push-domain/src/model.rs`, `lib.rs`, `store.rs`, `memory.rs`, `read.rs`, `conversations.rs`

Decision 6. `list_conversations` costs four to eight store calls per conversation today,
plus the `entries_after` misuse in `left_at_time`.

- [ ] **Step 1: Write the failing contract case**

Create `crates/push-testkit/src/contract/overviews.rs`:

```rust
use chrono::TimeDelta;
use push_domain::{
    ClientMsgId, Conversation, ConversationKind, ConversationOverview, DeviceId, Entry, EntryKind,
    GroupPositions, Member, Role, Store, Tx, UserId,
};

use super::{add_users, conversation, entry, member, start};
use crate::{Backend, Case, case_list};

/// Appends entries from these senders to `conversation`, moving `updated_at` to `at`.
async fn append<T: Tx>(
    tx: &mut T,
    conversation: &Conversation,
    senders: &[UserId],
    at: chrono::DateTime<chrono::Utc>,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    for sender in senders {
        let seq = tx.next_seq(conversation.id, at).await.unwrap();
        let entry = Entry {
            sender_id: *sender,
            ..entry(conversation.id, seq, ClientMsgId::new())
        };
        entries.push(entry);
    }
    entries
}

/// Alice is in three conversations: a direct one with bob, a group with carol (and dave,
/// who left), and a group she left.
async fn overviews_hold_what_a_listing_needs_for_every_membership<B: Backend>(backend: &B) {
    let store = backend.store().await;
    let [alice, bob, carol, dave] = [(); 4].map(|()| UserId::new());
    add_users(&store, &[alice, bob, carol, dave]).await;
    let (bobs_phone, bobs_laptop) = (DeviceId::new(), DeviceId::new());
    let mut tx = store.begin().await.unwrap();
    tx.insert_device(bobs_phone, bob).await.unwrap();
    tx.insert_device(bobs_laptop, bob).await.unwrap();

    // Direct: bob, alice, bob (deleted), bob, bob. Alice read 1, bob read 2.
    let direct = conversation();
    tx.insert_conversation(direct.clone()).await.unwrap();
    let at = start() + TimeDelta::hours(1);
    let mut entries = append(&mut tx, &direct, &[bob, alice, bob, bob, bob], at).await;
    entries[2].deleted_at = Some(start());
    for entry in entries {
        tx.insert_entry(entry).await.unwrap();
    }
    let alice_direct = Member {
        read_seq: 1,
        ..member(direct.id, alice)
    };
    let bob_direct = Member {
        read_seq: 2,
        ..member(direct.id, bob)
    };
    tx.insert_member(alice_direct.clone()).await.unwrap();
    tx.insert_member(bob_direct.clone()).await.unwrap();
    tx.raise_cursor(bobs_phone, direct.id, 3).await.unwrap();
    tx.raise_cursor(bobs_laptop, direct.id, 1).await.unwrap();
    let direct = Conversation {
        last_seq: 5,
        updated_at: at,
        ..direct
    };

    // Group: system entry by alice, then carol, alice. Dave left at 2.
    let group = Conversation {
        kind: ConversationKind::Group,
        title: Some("Trip".into()),
        ..conversation()
    };
    tx.insert_conversation(group.clone()).await.unwrap();
    let at = start() + TimeDelta::hours(2);
    let mut entries = append(&mut tx, &group, &[alice, carol, alice], at).await;
    entries[0].kind = EntryKind::System;
    for entry in entries {
        tx.insert_entry(entry).await.unwrap();
    }
    let alice_group = Member {
        role: Role::Admin,
        ..member(group.id, alice)
    };
    tx.insert_member(alice_group.clone()).await.unwrap();
    tx.insert_member(Member {
        read_seq: 3,
        ..member(group.id, carol)
    })
    .await
    .unwrap();
    tx.insert_member(Member {
        role: Role::Admin,
        left_at_seq: Some(2),
        ..member(group.id, dave)
    })
    .await
    .unwrap();
    let group = Conversation {
        last_seq: 3,
        updated_at: at,
        ..group
    };

    // Left: bob's system entry, alice leaving at 2 (half an hour in), then bob.
    let left = Conversation {
        kind: ConversationKind::Group,
        ..conversation()
    };
    tx.insert_conversation(left.clone()).await.unwrap();
    let at = start() + TimeDelta::hours(3);
    let mut entries = append(&mut tx, &left, &[bob, alice, bob], at).await;
    entries[0].kind = EntryKind::System;
    entries[1].kind = EntryKind::System;
    entries[1].server_ts = start() + TimeDelta::minutes(30);
    for entry in entries {
        tx.insert_entry(entry).await.unwrap();
    }
    let alice_left = Member {
        left_at_seq: Some(2),
        ..member(left.id, alice)
    };
    tx.insert_member(alice_left.clone()).await.unwrap();
    tx.insert_member(Member {
        role: Role::Admin,
        ..member(left.id, bob)
    })
    .await
    .unwrap();
    let left = Conversation {
        last_seq: 3,
        updated_at: at,
        ..left
    };
    tx.commit().await.unwrap();

    let sorted = |mut users: Vec<UserId>| {
        users.sort();
        users
    };
    let ids = [direct.id, group.id, left.id];
    let direct_overview = ConversationOverview {
        conversation: direct,
        member: alice_direct,
        members: sorted(vec![alice, bob]),
        admins: vec![],
        peer: Some((bob_direct, 3)),
        unread: 2,
        group: None,
        left_at: None,
    };
    let group_overview = ConversationOverview {
        conversation: group,
        member: alice_group,
        members: sorted(vec![alice, carol]),
        admins: vec![alice],
        peer: None,
        unread: 1,
        group: Some(GroupPositions {
            delivered: 1,
            read: 1,
        }),
        left_at: None,
    };
    let left_overview = ConversationOverview {
        conversation: left,
        member: alice_left,
        members: vec![bob],
        admins: vec![bob],
        peer: None,
        unread: 0,
        group: Some(GroupPositions {
            delivered: 3,
            read: 3,
        }),
        left_at: Some(start() + TimeDelta::minutes(30)),
    };
    let by_id = |mut overviews: Vec<ConversationOverview>| {
        overviews.sort_by_key(|overview| overview.conversation.id);
        overviews
    };

    let mut tx = store.begin().await.unwrap();
    assert_eq!(
        tx.conversation_overviews(alice, None, 100).await.map(by_id),
        Ok(by_id(vec![
            direct_overview.clone(),
            group_overview.clone(),
            left_overview.clone()
        ]))
    );
    assert_eq!(
        tx.conversation_overviews(alice, Some(start() + TimeDelta::minutes(90)), 100)
            .await
            .map(by_id),
        Ok(by_id(vec![group_overview, left_overview])),
        "only conversations changed after the given time"
    );
    let capped = by_id(tx.conversation_overviews(alice, None, 1).await.unwrap());
    let unread: Vec<_> = capped
        .iter()
        .map(|overview| (overview.conversation.id, overview.unread))
        .collect();
    let mut expected = vec![(ids[0], 1), (ids[1], 1), (ids[2], 0)];
    expected.sort();
    assert_eq!(unread, expected, "unread counts stop at the cap");
    assert_eq!(
        tx.conversation_overviews(UserId::new(), None, 100).await,
        Ok(vec![])
    );
}

pub(crate) fn cases<B: Backend>() -> Vec<Case<B>> {
    case_list![overviews_hold_what_a_listing_needs_for_every_membership]
}
```

In `crates/push-testkit/src/contract/mod.rs`, add `mod overviews;` after `mod entries;`, and add this line at the end of the `modules` list:

```rust
        ("contract::overviews", overviews::cases()),
```

- [ ] **Step 2: Run it to watch it fail**

```powershell
cargo test -p push-testkit
```

Expected: compile errors, `unresolved import `push_domain::ConversationOverview`` and `no method named `conversation_overviews` found`.

- [ ] **Step 3: Add the overview to the model and the contract**

In `crates/push-domain/src/model.rs`, add after the struct `GroupPositions`:

```rust
/// What a conversation listing needs about one of a user's memberships. The store reads
/// these for all of a user's memberships at once (`Tx::conversation_overviews`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationOverview {
    pub conversation: Conversation,
    /// The listing user's own member row.
    pub member: Member,
    /// The current members, sorted.
    pub members: Vec<UserId>,
    /// The current members with the admin role, sorted.
    pub admins: Vec<UserId>,
    /// A direct conversation's other member, with their `delivered_seq`. `None` for a
    /// group, or if there is no other member.
    pub peer: Option<(Member, i64)>,
    /// Live messages (`kind = Message`, not deleted) from others with
    /// `max(read_seq, visible_from_seq - 1) < seq <= member.visible_upper(last_seq)`, at
    /// most the cap given to the store.
    pub unread: u32,
    /// A group's group-wide positions with its log ending at its `last_seq`; `None` for a
    /// direct conversation or a group without current members.
    pub group: Option<GroupPositions>,
    /// For a former member, the `server_ts` of the entry at their `left_at_seq`; `None`
    /// for a current member, or if that entry does not exist.
    pub left_at: Option<DateTime<Utc>>,
}
```

In `crates/push-domain/src/lib.rs`, replace the `pub use model::{…};` statement with:

```rust
pub use model::{
    ContactState, ContactSummary, ContentType, Conversation, ConversationKind,
    ConversationOverview, ConversationState, ConversationSummary, Entry, EntryKind, GroupPositions,
    Invite, InviteRecord, Member, MemberState, Positions, Receipts, Role, SendReceipt, direct_key,
};
```

In `crates/push-domain/src/store.rs`, replace `use crate::model::{…};` with:

```rust
use crate::model::{
    ContactState, Conversation, ConversationOverview, Entry, GroupPositions, InviteRecord, Member,
};
```

and add after the declaration of `group_positions` (the last method of the trait):

```rust
    /// An overview of each conversation `user` is, or was, a member of whose `updated_at`
    /// is later than `changed_since` (every one without it). Unread counts stop at
    /// `unread_cap`. Each conversation is read in the same statement as the user's member
    /// row, as in `memberships`. One statement on Postgres, however many conversations
    /// there are. The order of the result is unspecified.
    async fn conversation_overviews(
        &mut self,
        user: UserId,
        changed_since: Option<DateTime<Utc>>,
        unread_cap: u32,
    ) -> DomainResult<Vec<ConversationOverview>>;
```

- [ ] **Step 4: Implement it in memory**

In `crates/push-domain/src/memory.rs`, replace `use crate::model::{…};` with:

```rust
use crate::model::{
    ContactState, Conversation, ConversationKind, ConversationOverview, Entry, EntryKind,
    GroupPositions, InviteRecord, Member, Role,
};
```

Replace the whole `impl State { … }` block with this one, which adds `count_incoming_messages`, `group_positions` and `overview`:

```rust
impl State {
    fn first_live_message(
        &self,
        conversation: ConversationId,
        after: i64,
        up_to: i64,
        not_from: Option<UserId>,
    ) -> Option<i64> {
        if after >= up_to {
            return None;
        }
        self.entries
            .range((conversation, after + 1)..=(conversation, up_to))
            .map(|(_, entry)| entry)
            .find(|entry| {
                entry.kind == EntryKind::Message
                    && entry.deleted_at.is_none()
                    && not_from.is_none_or(|user| entry.sender_id != user)
            })
            .map(|entry| entry.seq)
    }

    fn delivered_seq(&self, conversation: ConversationId, user: UserId) -> i64 {
        self.cursors
            .iter()
            .filter(|((device, cursor_conversation), _)| {
                *cursor_conversation == conversation
                    && self.devices.get(device).is_some_and(|row| row.user == user)
            })
            .map(|(_, position)| *position)
            .max()
            .unwrap_or(0)
    }

    fn count_incoming_messages(
        &self,
        conversation: ConversationId,
        user: UserId,
        after: i64,
        up_to: i64,
        cap: u32,
    ) -> u32 {
        if after >= up_to {
            return 0;
        }
        self.entries
            .range((conversation, after + 1)..=(conversation, up_to))
            .map(|(_, entry)| entry)
            .filter(|entry| {
                entry.kind == EntryKind::Message
                    && entry.deleted_at.is_none()
                    && entry.sender_id != user
            })
            .take(cap as usize)
            .count() as u32
    }

    fn group_positions(
        &self,
        conversation: ConversationId,
        last_seq: i64,
    ) -> Option<GroupPositions> {
        self.members
            .values()
            .filter(|member| member.conversation_id == conversation && member.is_current())
            .map(|member| {
                let delivered_seq = self.delivered_seq(conversation, member.user_id);
                GroupPositions {
                    delivered: self.effective_position(
                        member,
                        last_seq,
                        delivered_seq.max(member.read_seq),
                    ),
                    read: self.effective_position(member, last_seq, member.read_seq),
                }
            })
            .reduce(|all, one| GroupPositions {
                delivered: all.delivered.min(one.delivered),
                read: all.read.min(one.read),
            })
    }

    /// See [`Tx::conversation_overviews`].
    fn overview(
        &self,
        conversation: &Conversation,
        member: &Member,
        unread_cap: u32,
    ) -> ConversationOverview {
        let id = conversation.id;
        let rows: Vec<&Member> = self
            .members
            .values()
            .filter(|row| row.conversation_id == id)
            .collect();
        let current = |admins_only: bool| -> Vec<UserId> {
            rows.iter()
                .filter(|row| row.is_current() && (!admins_only || row.role == Role::Admin))
                .map(|row| row.user_id)
                .collect()
        };
        let peer = (conversation.kind == ConversationKind::Direct)
            .then(|| rows.iter().find(|row| row.user_id != member.user_id))
            .flatten()
            .map(|peer| ((*peer).clone(), self.delivered_seq(id, peer.user_id)));
        let left_at = member
            .left_at_seq
            .and_then(|seq| self.entries.get(&(id, seq)).map(|entry| entry.server_ts));
        ConversationOverview {
            conversation: conversation.clone(),
            member: member.clone(),
            members: current(false),
            admins: current(true),
            peer,
            unread: self.count_incoming_messages(
                id,
                member.user_id,
                member.read_seq.max(member.visible_from_seq - 1),
                member.visible_upper(conversation.last_seq),
                unread_cap,
            ),
            group: (conversation.kind == ConversationKind::Group)
                .then(|| self.group_positions(id, conversation.last_seq))
                .flatten(),
            left_at,
        }
    }

    /// See [`Tx::group_positions`].
    fn effective_position(&self, member: &Member, last_seq: i64, position: i64) -> i64 {
        let upper = member.visible_upper(last_seq);
        let from = position.max(member.visible_from_seq - 1);
        if from >= upper {
            return upper;
        }
        self.first_live_message(member.conversation_id, from, upper, Some(member.user_id))
            .map_or(upper, |seq| seq - 1)
    }
}
```

In `impl Tx for InMemoryTx`, replace `count_incoming_messages` and `group_positions` with these, which call the helpers, and add `conversation_overviews` at the end:

```rust
    async fn count_incoming_messages(
        &mut self,
        conversation: ConversationId,
        user: UserId,
        after: i64,
        up_to: i64,
        cap: u32,
    ) -> DomainResult<u32> {
        Ok(self
            .work
            .count_incoming_messages(conversation, user, after, up_to, cap))
    }

    async fn group_positions(
        &mut self,
        conversation: ConversationId,
        last_seq: i64,
    ) -> DomainResult<Option<GroupPositions>> {
        Ok(self.work.group_positions(conversation, last_seq))
    }

    async fn conversation_overviews(
        &mut self,
        user: UserId,
        changed_since: Option<DateTime<Utc>>,
        unread_cap: u32,
    ) -> DomainResult<Vec<ConversationOverview>> {
        let state = &self.work;
        Ok(state
            .members
            .values()
            .filter(|member| member.user_id == user)
            .filter_map(|member| {
                let conversation = state.conversations.get(&member.conversation_id)?;
                if changed_since.is_some_and(|since| conversation.updated_at <= since) {
                    return None;
                }
                Some(state.overview(conversation, member, unread_cap))
            })
            .collect())
    }
```

- [ ] **Step 5: List conversations through it**

In `crates/push-domain/src/read.rs`, replace the function `group_positions_for` (with its doc comment) with a pure function:

```rust
/// A group's group-wide positions `all` as `member` may see them: never above the end of
/// their visible range, and zero for a group without current members.
pub(crate) fn visible_group_positions(
    all: Option<GroupPositions>,
    conversation: &Conversation,
    member: &Member,
) -> GroupPositions {
    let upper = member.visible_upper(conversation.last_seq);
    all.map_or(
        GroupPositions {
            delivered: 0,
            read: 0,
        },
        |all| GroupPositions {
            delivered: all.delivered.min(upper),
            read: all.read.min(upper),
        },
    )
}
```

and in `conversation_state` replace `let group = group_positions_for(&mut tx, &found, &caller).await?;` with:

```rust
            let all = group_positions(&mut tx, &found, found.last_seq).await?;
            let group = visible_group_positions(all, &found, &caller);
```

In `crates/push-domain/src/conversations.rs`, replace the imports above the first `impl` block with:

```rust
use std::collections::HashSet;

use chrono::{DateTime, Utc};

use crate::clock::Clock;
use crate::contacts::are_contacts;
use crate::error::{DomainError, DomainResult};
use crate::ids::{ConversationId, UserId};
use crate::model::{
    ContactState, Conversation, ConversationKind, ConversationOverview, ConversationSummary,
    Member, Receipts, Role, direct_key,
};
use crate::notify::Notifier;
use crate::read::visible_group_positions;
use crate::service::Messaging;
use crate::store::{InsertOutcome, Store, Tx};
use crate::validation::{MAX_UNREAD, ValidationError};
```

replace the body of `list_conversations` (its doc comment does not change) with the version below, and delete the functions `left_at_time`, `receipts`, `current_admin_ids` and `has_blocked_a_member` (with their doc comments), which nothing uses any more:

```rust
    pub async fn list_conversations(
        &self,
        user: UserId,
        changed_since: Option<DateTime<Utc>>,
    ) -> DomainResult<Vec<ConversationSummary>> {
        let mut tx = self.store.begin().await?;
        let mut overviews = tx
            .conversation_overviews(user, changed_since, MAX_UNREAD)
            .await?;
        let blocked: HashSet<UserId> = tx
            .contacts_of(user)
            .await?
            .into_iter()
            .filter(|(_, state)| *state == ContactState::Blocked)
            .map(|(peer, _)| peer)
            .collect();
        tx.rollback().await?;
        overviews.sort_by_key(|overview| overview.conversation.id);

        let mut summaries = Vec::new();
        for overview in overviews {
            // Blocking hides a direct conversation from the blocker.
            if overview
                .peer
                .as_ref()
                .is_some_and(|(peer, _)| blocked.contains(&peer.user_id))
            {
                continue;
            }
            let ConversationOverview {
                conversation,
                member,
                members,
                admins,
                peer,
                unread,
                group,
                left_at,
            } = overview;
            let receipts = if conversation.kind == ConversationKind::Group {
                let group = visible_group_positions(group, &conversation, &member);
                Receipts::Group {
                    all_delivered_seq: group.delivered,
                    all_read_seq: group.read,
                }
            } else {
                let (peer_delivered_seq, peer_read_seq) = peer
                    .map_or((0, 0), |(peer, delivered_seq)| {
                        (delivered_seq.max(peer.read_seq), peer.read_seq)
                    });
                Receipts::Direct {
                    peer_delivered_seq,
                    peer_read_seq,
                }
            };
            let mut summary = ConversationSummary {
                conversation_id: conversation.id,
                kind: conversation.kind,
                title: conversation.title.clone(),
                last_seq: member.visible_upper(conversation.last_seq),
                read_seq: member.read_seq,
                unread,
                receipts,
                updated_at: conversation.updated_at,
                members,
                admins,
            };
            if member.left_at_seq.is_some() {
                // A former member sees the conversation as it was when they left.
                summary.updated_at = left_at.unwrap_or(conversation.created_at);
                if changed_since.is_some_and(|since| summary.updated_at <= since) {
                    continue;
                }
                summary.members.clear();
                summary.admins.clear();
            }
            summaries.push(summary);
        }
        Ok(summaries)
    }
```

- [ ] **Step 6: Run the gates and commit**

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

Expected: 271 tests pass (22 unit, 249 `in_memory`), no warnings, no diff, the 1.85 build succeeds. Every `listings::…` scenario (blocked chats hidden, former members, unread caps, group receipts) passes unchanged.

```powershell
git add -A crates
git commit -m "feat(domain): list conversations with a constant number of store calls"
```

---

### Task 10: Spec, roadmap and pull request

**Files:**
- Modify: `docs/superpowers/specs/2026-10-11-push-messenger-design.md`
- Modify: `docs/superpowers/plans/2026-10-11-roadmap.md`

The CI workflow needs no change: `cargo test --workspace` already runs the new
`in_memory` target, and both jobs build on 1.85.

- [ ] **Step 1: Update the spec**

In §10, in the repository layout, add after the `push-domain` line:

```
│   ├── push-testkit      the domain's behaviour suite and storage contract, run
│   │                     against every store (not published)
```

In §12, replace the `Unit` and `Integration` rows of the table with:

```markdown
| Unit | `push-domain` with in-memory repositories: seq allocation, retry replay, positions, receipts, edit/delete rules, contacts, group membership, validation. The scenarios and the storage contract live in `push-testkit` and are written once: every store runs all of them |
| Integration | Real Postgres and Redis via testcontainers: the whole `push-testkit` suite against Postgres, transaction order, gapless `seq` under concurrent retries, outbox relay, `SKIP LOCKED` |
```

- [ ] **Step 2: Split plan 3 in the roadmap**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, replace the row for plan 3 (`| 3 | Postgres store | … | 1, 2A, 2B | |`) with:

```markdown
| 3A | Conformance suite and set-based storage | `push-testkit`: the domain scenarios and the storage contract written once for every store; set-based `Tx` methods for group positions, listings, sync and conversation state; a per-conversation position lock; race-free device registration | 2B | In review |
| 3B | Postgres store | `push-store`: migrations and the storage traits on Postgres (sqlx); the whole `push-testkit` suite on testcontainers, plus concurrency tests | 3A | |
```

and in the row for plan 4, replace the dependency `3` with `3B` (the row then ends `| 3B | |`).

- [ ] **Step 3: Run the workspace gates**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 test --workspace --locked
```

Expected: no formatting diff, no warnings, 271 tests pass on both toolchains (`push-domain` 22 unit, `push-testkit` `in_memory` 249).

- [ ] **Step 4: Commit and push**

```powershell
git add docs
git commit -m "docs: record plan 3A in the spec and roadmap"
git push -u origin feature/plan-03a-conformance
```

- [ ] **Step 5: Open the pull request and wait for CI**

```powershell
gh pr create --base main --title "Plan 3A: conformance suite and set-based storage" --body "Implements docs/superpowers/plans/2026-10-12-plan-03a-conformance-suite.md: push-testkit runs the 199 domain scenarios and a 50-case Tx contract against any store (libtest-mimic harness; in-memory now, Postgres in plan 3B); race-free device registration; monotonic updated_at; a per-conversation lock for group receipt positions; set-based Tx methods for group positions, member positions, sync positions and conversation overviews, so listings, sync, conversation state and group receipts take a constant number of store calls."
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust` and `msrv` jobs). Do not merge; the controller merges after the final review.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §12 unit layer: the domain rules tested with in-memory repositories, written once for every store | 1, 2, 3 |
| §3.2 every `Tx` method's contract checked by conformance cases (keys, duplicates, ranges, positions) | 3, 4, 5, 6, 7, 8, 9 |
| §4.3 a device registered concurrently with another user cannot take over the device id | 4 |
| §4.5 `ListConversations{changed_since}`: `updated_at` never moves back | 4 |
| §4.5 group receipts are not lost when members ack or read at the same time (per-conversation positions lock) | 5 |
| §4.5 group-wide minimums in one store call | 6 |
| §4.3 sync from every membership's position in one store call; missing positions start at the floor | 8 |
| §4.4/§4.5 direct receipt positions and conversation state in one store call | 7 |
| §4.5 listings with receipt positions, unread counts and former members in a constant number of store calls | 9 |
| Plan 2B notes for Plan 3: single-statement listings, sync positions, conversation state and group positions; `left_at_time` as a point lookup; the device registration race; monotonic `updated_at`; group receipt concurrency decided | 4, 5, 6, 7, 8, 9 |

## Deferred to later plans

- **Postgres** — all of it: Plan 3B implements every `Tx` method on PostgreSQL, runs the
  whole `push-testkit` suite against it, and adds the concurrency scenarios the in-memory
  store cannot run (gapless concurrent sends, concurrent retries, concurrent
  `open_direct`, block versus accept, invite use limits, concurrent group acks, deadlocks
  reported as `Conflict`).
- `register_device` still creates one delivery position per membership, and `join` one per
  device of each new member (`create_group` with 99 others: about two statements per
  member). Both are rare operations; a set-based `create_cursors` can replace them when a
  profile shows the need.
- Plan 2B deferrals that remain open: a former member sees the current title; `create_group`
  has no idempotency key; group changes carry no device; redeem rate limiting (Plan 8); the
  wire mapping of operations and events (Plans 4, 7, 8).
