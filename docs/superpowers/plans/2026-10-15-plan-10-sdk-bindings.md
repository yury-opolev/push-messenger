# Plan 10 — SDK bindings: Node.js and C Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Two bindings over `push-sdk` (spec §8.1, §8.5), so apps outside Rust can use it, and nothing in them but conversion and waiting. **Node.js** (`bindings/node`, napi-rs): every call that talks to a server or touches the database returns a promise settled from the client's own runtime, events reach one listener through a threadsafe function, the app's secret store is a JavaScript object (with promises or without) bridged so that neither the JavaScript thread nor an SDK thread can wait for the other in a circle, failures are `PushError`s with the SDK's stable `code`, and `index.d.ts` is checked by `tsc`; tests run in `node:test` against the SDK's fake auth-svc and a fake gateway that answers by itself. **C** (`bindings/c`, cbindgen): a stable C ABI — opaque handles with explicit create and destroy, `PUSH_OK`/`PUSH_ERROR` with an error object carrying the same codes, UTF-8 strings in (borrowed) and out (owned, `push_string_free`), compound data as JSON, blocking calls bounded by the SDK's own time limits, events by callback or by polling, the secret store as C function pointers; the generated header is checked in and a test fails when it is stale; a C program compiled with MSVC or gcc runs a whole flow. Both share one crate (`bindings/ffi`) that shapes the data and the error codes, so they are identical in Rust, Node and C. A Node.js example runs against the minikube cluster through its ingress; the smoke test still passes.

**Architecture:** `push-sdk` gains three small things for the bindings and no behaviour: `Client::spawn` (start a future on the client's runtime, so a binding needs no runtime of its own — spec §8.5: one runtime, owned by the client), `DatabaseKey::as_bytes` (to hand a derived key to the app), and its test doubles behind a `testing` feature with a new `AutoGateway` and a `push-sdk-fakes` binary. `bindings/ffi` (`push-sdk-ffi`) holds the JSON shapes of every type, event and error, and `block_on`, a thread-parking wait for the client's futures. `bindings/c` (`push-sdk-c`) is a `cdylib` of `extern "C"` functions, each behind a `catch_unwind` guard; `bindings/node` (`push-sdk-node`) is a napi-rs `cdylib` wrapped by `index.js`. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §8, §10, §12.

**Tech Stack:** Rust 2024 (toolchain ≥ 1.85). napi 3.4.0, napi-derive 3.3.0 and napi-build 2.2.4 — the last releases that build with Rust 1.85 (later ones need 1.88); the workspace's resolver 3 is MSRV-aware and picks them, and the lockfile pins them. cbindgen 0.29.4 (Rust 1.74) as a dev-dependency, `cc` 1 to compile the C test program. Node.js 18 or later (verified with 18.20 and 22.14), `node:test`, TypeScript 5.9.3 (dev only, for `tsc`). MSVC on Windows, gcc in CI.

---

## Before you start

- **Plan 9B is merged** (PR #17). This plan adds `bindings/ffi`, `bindings/c` and
  `bindings/node`, changes `push-sdk` (Task 1), the CI workflow, the README, two guides,
  `.dockerignore`, and adds `deploy/minikube/node-example.ps1`.
- **The spec is already revised.** `docs/superpowers/specs/2026-10-11-push-messenger-design.md`
  on `main` describes the bindings as built here (committed with this plan), so no task
  changes it; Task 9 only marks the roadmap.
- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there unless it says `cd`. Create the branch from an up-to-date
  `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-10-sdk-bindings
```

- Fix a stale PATH for the current PowerShell session (each new shell needs it again), and
  build without incremental artifacts:

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
$env:CARGO_INCREMENTAL = '0'
```

- **Node.js and npm must be installed** (`node --version`: 18 or later; verified with
  22.14 on Windows and 18.20 on Linux). **A C compiler** is needed for the C test: MSVC
  (Visual Studio Build Tools; the `cc` crate finds it without a developer prompt) or gcc.
- **Docker must be running** for the workspace gates (other crates' tests start
  `postgres:17-alpine` and `redis:7-alpine`). The bindings' tests need no Docker.
- **How the code is given.** Every code block of a task was pasted from the commit that
  implemented and verified it. A new file is given whole ("Create"). A changed file is
  given as a git patch ("Change"): save the block to a file outside the tree and run
  `git apply <file>` from the repository root, in the order the task lists them. A new
  Markdown file is given as a patch too, because it contains code blocks of its own.
  `bindings/c/include/push_sdk.h` is not pasted: it is generated (Tasks 3 and 4 say how)
  and its SHA-256 is given.
- **Keep files LF** (`.gitattributes`: `* text=auto eol=lf`).
- **Do not pipe `cargo fmt`.** Run `cargo fmt --all -- --check` on its own and read the exit
  code.
- Baseline (after Plan 9B): `cargo test --workspace --locked` passes 966 tests.
- **Cargo.lock.** Tasks 2 to 5 carry their lockfile changes in their patches; never run
  `cargo update`.
- **Red, then green.** Step 2 of every code task breaks one rule on purpose, runs the
  task's tests and expects the named test to fail, then restores the file. Task 7 writes its
  test first instead; Task 8's red step runs on the cluster.
- **Every task ends green:**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

  and, from Task 5 on, in `bindings/node`: `npm ci`, `npm test` and (from Task 6)
  `npm run typecheck`.
- **Disk space.** Watch the target directory; run `cargo clean` if it passes ~30 GB.
- **Scope rule:** no change to any service, the chart or the smoke test; `push-sdk` gains
  only `Client::spawn`, `DatabaseKey::as_bytes` and the `testing` feature; no group calls
  (owner decision); no keychain store (owner decision: the guide shows apps how); nothing
  is published.

## Key rules the code must preserve

- **One runtime.** The bindings never build a tokio runtime: Node settles promises from the
  client's runtime (`Client::spawn`); C and Node's helper threads wait with
  `push_sdk_ffi::block_on`, which only parks the thread (spec §8.5).
- **The JavaScript thread never waits for the SDK, and an SDK thread never waits for the
  JavaScript thread while the JavaScript thread waits for it.** Reads of the secret store
  are served from memory after `Client.open` loaded them; writes wait on an SDK thread
  (never on the JavaScript thread, which is refused) for the store's answer, up to 30 s.
- **Credentials are saved before they are used:** the Node bridge changes its copy only
  after the app's store confirmed the write.
- **No panic crosses the C boundary** (every `extern "C"` function runs in `catch_unwind`;
  a panic is the code `panic`). Every allocation returned to C has a free function. No
  output is written after a failure that the caller cannot see: output pointers are
  checked before any work.
- **One set of error codes.** Both bindings take `code` and `retryable` from
  `SdkError::code` and `SdkError::is_retryable` through `push_sdk_ffi::json::error`; the
  guide lists every code and a test keeps it complete.
- **Secrets stay where they are.** The database key passes as bytes, never inside JSON
  (an unknown config field is refused); no binding logs a token, a password, a key or a
  message's text; the examples print lengths, ids and seqs.
- **Blocking C calls are never made on an SDK thread** or from inside a secret store
  function: they are refused with `invalid`.

## Decisions made for this plan

These were settled while writing the plan and verified in a worktree and on minikube. Do
not reopen them during execution.

1. **Layout (spec §10).** `bindings/ffi` (`push-sdk-ffi`, shared), `bindings/c`
   (`push-sdk-c`) and `bindings/node` (`push-sdk-node`), all workspace members. The shared
   crate keeps the two bindings from drifting: one function shapes each type, event and
   error.
2. **The core changes only for the bindings' needs.** `Client::spawn` starts a future on the
   client's runtime and returns at once (the Node binding settles its promises from there);
   `DatabaseKey::as_bytes` hands a derived key to an app; the test doubles move behind a
   `testing` feature (never for apps), with `AutoGateway` — a gateway that answers by
   itself: `Ready`, `Pong`, a `SendAck` with the conversation's next `seq` for every send,
   edit and delete, the peer's copy of every text delivered next, `OpenDirect`,
   `CreateInvite`, `RedeemInvite` (`NOSUCHCODE` is `invite_not_found`) and empty listings,
   `rejected` for anything else — and FakeAuth answering sign-up, resend and reset
   requests, and refusing the password `wrong password` with `invalid_credentials`.
   `push-sdk-fakes` (a binary needing the feature) prints the fakes' URLs as one JSON line
   for the Node tests. `Client::open` stays synchronous: the bindings open on a thread of
   their own (Node) or on the caller's thread, documented as slow (C) — the review note on
   `open` is honoured without an `open_async`.
3. **The Node secret store bridge** (`bindings/node/src/secrets.rs`). The SDK's
   `SecretStore` is three blocking calls made on SDK threads; a JavaScript store answers on
   the JavaScript thread, usually with promises. A blocking call into JavaScript while the
   JavaScript thread waits for the same operation would deadlock. The bridge therefore
   (a) loads the three keys once, on the opening thread, before `Client::open`, while the
   JavaScript thread is free (it awaits `open`'s promise); (b) answers every later `load`
   from that copy, so the synchronous calls JavaScript makes (`account()`) never reach
   JavaScript; (c) sends `save` and `delete` through a weak threadsafe function and waits
   for `completeSecretCall(id, error, value)` on the SDK thread (through
   `tokio::task::block_in_place` on a runtime worker, so its other tasks move), up to 30 s,
   changing the copy only after the store confirmed — so credentials are still saved
   before use; (d) refuses a write on the JavaScript thread instead of deadlocking (none
   happens: every call that writes runs on an SDK thread and returns a promise). Rejected:
   an async `SecretStore` (a rewrite of the core's credentials), a write-behind cache that
   does not wait (breaks "saved before used" and loses failures), a blocking threadsafe
   call from the JavaScript thread (deadlock). Proven by the Node tests: an asynchronous
   store answering after 20 ms while the JavaScript thread calls `account()` every
   millisecond during the sign-in; a save that throws or rejects fails the sign-in with
   `secrets` and keeps the registration id; a reopened client knows its account at once;
   and Task 5's red step (reads going to JavaScript fail the first test).
4. **Node events** go to one listener per client through a threadsafe function with a
   queue of 1,000 calls, fed by a thread of the binding (`push-sdk-events`) in blocking
   mode: when JavaScript falls behind the thread waits and the SDK's own bounded queue
   (10,000, then `eventsLost`) takes over, so nothing grows without bound. The listener
   keeps Node running, like an open socket, until `close()`; a test runs a child process
   that must exit after `close()`. A promise whose task is dropped (the client closed while
   the call ran) rejects with `closed` (`Settle`'s `Drop`), never stays pending.
5. **Node errors and data.** Native calls reject or throw an `Error` whose message is the
   error's JSON; `index.js` turns it into `PushError {code, message, retryable,
   retryAfterS}`; an argument of the wrong type (napi's own error) is `invalid`. Data is
   the shared JSON, converted to objects by napi's `serde-json` feature.
   `account()`, `connectionState()`, `connect()`, `disconnect()` and `onEvent()` answer at
   once; all other calls (local reads included, so the JavaScript thread never waits for the
   database's lock) return promises.
6. **C: blocking calls.** A call that talks to a server blocks the calling thread until it
   ends; the SDK bounds each (requests 15 s for the connection plus 15 s for the answer;
   auth-svc calls 15 s per attempt, at most three retries). Chosen over completion
   callbacks because C apps have threads of their own, results stay on the caller's stack
   with plain ownership, no context pointer has to outlive a call on an SDK thread, and
   the API stays one function per call. The wait is `block_on` on the calling thread (no
   second runtime); on an SDK runtime thread it is refused with `invalid`, and every call
   from inside a secret store function is refused (a thread-local flag), so neither can
   wait for itself.
7. **C strings and data.** Arguments: UTF-8, NUL-terminated, borrowed for the call; NULL
   or invalid UTF-8 is `invalid`. Results: `char *` the caller owns and frees with
   `push_string_free`, UTF-8 JSON for anything with fields (the same JSON as Node's
   objects) and plain strings for ids. JSON over the ABI keeps it stable as the SDK grows
   (a new field is not a new struct layout, no versioned structs) — see Owner decisions
   needed, 1. Errors are `PushError` objects with borrowed accessors and
   `push_error_free`; every result pointer is checked before the call acts.
8. **C events:** a callback with a context pointer on a thread of the binding
   (`push-sdk-events`), one event at a time and in order, with a `release` callback after
   the last one (spec §8.5); or `push_client_next_event` with a timeout. One or the other
   per client. The callback may call the SDK, blocking calls and `push_client_destroy`
   included (it is not a runtime thread).
9. **C secret store:** `PushSecretStore {context, load, save, remove, free_value,
   release}`. `load` returns a buffer the app allocated, which the SDK copies and hands
   back to `free_value` (no allocator crosses the boundary); `release` runs once when the
   client no longer needs the store, also when `push_client_open` fails. The functions must
   be thread-safe.
10. **The header** is generated by cbindgen from the crate (`cbindgen.toml`, C99 comments,
    `cpp_compat`) and checked in; `tests/header.rs` regenerates it in memory and fails when
    the file differs, so `cargo test` in CI (the `rust`, `msrv` and `bindings` jobs) is the
    up-to-date check. `PUSH_SDK_BLESS_HEADER=1` writes it. Verified identical on Windows and
    Linux.
11. **The C test** (`tests/c_program.rs`) builds the shared library with a nested
    `cargo build -p push-sdk-c --lib` (cargo builds only the rlib for integration tests),
    compiles `tests/c/flow.c` with the `cc` crate's compiler (MSVC or gcc) against the
    header, starts the fakes in-process and runs the program. On Linux it found a real bug
    in the first draft: C leaves argument evaluation order open, so the error pointer must
    be read after the call (the program takes its address).
12. **The Node crate has no Rust tests** (`test = false`): its test binary could not resolve
    Node's symbols; its tests run in Node (`npm test`), and its lints are its own (napi's
    macros generate `unsafe`, which the workspace forbids). The C crate denies
    `unsafe_op_in_unsafe_fn` and allows `missing_safety_doc` (one safety contract for every
    function, stated in the crate docs and the header).
13. **Packaging:** `bindings/node/package.json` (`push-sdk`, private, `index.js`,
    `index.d.ts`, `push_sdk.node`), built by `scripts/build.js` (cargo, then a copy);
    nothing is published. The C library is `push_sdk_c.dll`/`libpush_sdk_c.so`/`.dylib`.
14. **CI:** a `bindings` job on ubuntu-latest: Rust stable, Node 22, `cargo test -p
    push-sdk-c -p push-sdk-ffi` (header, C program with gcc), `npm ci`, `npm test`,
    `npm run typecheck`. The `rust` and `msrv` jobs run the C tests too.
15. **Minikube:** the chart and the smoke test do not change; `deploy/minikube/up.ps1
    -Smoke` proves nothing regressed, and `deploy/minikube/node-example.ps1` forwards local
    ports to the ingress and to the DEV ONLY mailbox and runs
    `bindings/node/examples/minikube.js`: two Node apps sign up, verify, sign in, become
    contacts by invite and chat with a read receipt. `.dockerignore` keeps `node_modules`
    and built addons out of the image context.

## Owner decisions

Plan 9's decisions apply and are what the bindings expose: token storage is left to apps
(the bindings take the store as callbacks; `docs/sdk-bindings.md` wires Node to the OS
keychain; no store is shipped), the database key is the app's (bytes, never logged),
groups are not exposed, and a server sign-out keeps local data.

## Owner decisions needed

None of these blocks Plan 10: each has a default the plan already implements or defers.

1. **How C apps receive data: JSON text, or C structs?** (the plan does JSON; please
   confirm or ask for structs)

   A C app that calls `push_client_messages` needs the messages' fields. There are two ways
   to hand them over:

   - **JSON text (built):** the call returns one string, for example
     `[{"clientMsgId":"0199…","seq":4,"body":"hi","status":"read",…}]`, which the app
     parses with any JSON library and frees with `push_string_free`. Adding a field later
     (say `reactions`) changes nothing for existing apps: they ignore keys they do not know.
     The same JSON is what the Node binding gives JavaScript, so both are documented once.
   - **C structs:** the call returns `PushMessage *messages, size_t count`, with fields
     like `int64_t seq` and `const char *body`, freed with `push_messages_free`. Faster and
     typed, with no JSON library needed, but every new field changes the struct's layout:
     an app compiled against the old header would read garbage from a new library unless
     the API adds versioned structs (`PushMessageV2`) or size fields, and there are about
     ten types to keep in step.

   Recommendation: keep JSON for the first release (it is what most apps will wrap anyway,
   e.g. Swift's `Codable` or Kotlin's serialization), and add typed structs later only if a
   real C or C++ app asks for them.

2. **Mobile apps: is the C library enough, or do you want Swift and Kotlin bindings?**

   Third parties will build iPhone and Android apps. Today they would call the C library:
   from Swift through a module map over `push_sdk.h`, from Kotlin through hand-written
   JNI. That works, but every app repeats the same glue (JSON decoding, threads,
   callbacks). The alternative is a later plan that generates native Swift and Kotlin
   packages from the Rust core with Mozilla's UniFFI: an iOS developer would write
   `try await client.send(conversationId: id, text: "hi")` and get a Swift struct back.
   Either way the SDK must also be cross-compiled for iOS (arm64, simulator) and Android
   (arm64-v8a, x86_64), including SQLite3 Multiple Ciphers — not done yet.

   Please say whether mobile matters for the first release, and if so whether apps should
   use the C library (cheaper now) or get Swift/Kotlin packages (a plan of its own,
   roughly the size of this one).

3. **Publishing: names and platforms.** Nothing is published yet. When it is:

   - **npm package name.** The package is called `push-sdk` locally; that name may be
     taken on npm, and a scope such as `@push-messenger/sdk` (needs an npm organisation)
     is safer. Which name?
   - **Prebuilt binaries.** An npm user should not need Rust. The usual way is one small
     package per platform with the compiled addon (`@push-messenger/sdk-win32-x64-msvc`,
     `-linux-x64-gnu`, `-darwin-arm64`, …) built in CI. Likewise the C library as release
     assets (`push_sdk_c.dll`, `libpush_sdk_c.so`, `libpush_sdk_c.dylib` and the header).
     Which platforms do you want first (for example Windows x64, Linux x64, macOS arm64)?
   - **Node versions.** The binding works on Node 18 (verified) and 22 (verified); 18
     reached end of life in April 2025, so a published package would say `>=20`. Agree?

## File structure

```
crates/push-sdk/Cargo.toml                    feature `testing`, the push-sdk-fakes binary
crates/push-sdk/src/client.rs                 Client::spawn
crates/push-sdk/src/key.rs                    DatabaseKey::as_bytes
crates/push-sdk/src/lib.rs, testing.rs        the test doubles, public behind `testing`; AutoGateway
crates/push-sdk/src/bin/push-sdk-fakes.rs     NEW: the fakes for the Node tests
bindings/ffi/                                 NEW push-sdk-ffi: json.rs (shapes, errors, config), block_on
bindings/c/                                   NEW push-sdk-c: src/lib.rs, src/client.rs, include/push_sdk.h,
                                              cbindgen.toml, build.rs, tests/header.rs, tests/c_program.rs,
                                              tests/c/flow.c
bindings/node/                                NEW push-sdk-node: src/{lib,client,secrets,promise}.rs,
                                              index.js, index.d.ts, package.json, package-lock.json,
                                              tsconfig.json, scripts/build.js, test/*, examples/minikube.js
deploy/minikube/node-example.ps1              NEW: the Node example on the cluster
docs/sdk-bindings.md                          NEW: the guide
Cargo.toml, Cargo.lock, .github/workflows/ci.yml, README.md, docs/sdk-secure-storage.md, .dockerignore
```

---

### Task 1: The core's hooks for the bindings, and fakes they can test against

**Files:**
- Modify: `crates/push-sdk/Cargo.toml`, `crates/push-sdk/src/client.rs`,
  `crates/push-sdk/src/key.rs`, `crates/push-sdk/src/lib.rs`,
  `crates/push-sdk/src/testing.rs`
- Create: `crates/push-sdk/src/bin/push-sdk-fakes.rs`

`Client::spawn` lets a binding settle a promise from the client's runtime;
`DatabaseKey::as_bytes` lets it hand a derived key to the app; the test doubles become
public behind `testing`, with a gateway that answers by itself.

- [ ] **Step 1: Write the code**

Change `crates/push-sdk/Cargo.toml` (save the block to a file and `git apply` it):

```diff
diff --git a/crates/push-sdk/Cargo.toml b/crates/push-sdk/Cargo.toml
index 516ef4e..8b1e0c8 100644
--- a/crates/push-sdk/Cargo.toml
+++ b/crates/push-sdk/Cargo.toml
@@ -7,9 +7,19 @@ rust-version.workspace = true
 license.workspace = true
 repository.workspace = true
 
+[features]
+# The test doubles (`testing`) and the `push-sdk-fakes` binary, for the bindings' tests.
+# Never for an app.
+testing = ["dep:axum"]
+
+[[bin]]
+name = "push-sdk-fakes"
+required-features = ["testing"]
+
 [dependencies]
 # Argon2id for `DatabaseKey::derive`, as auth-svc hashes passwords.
 argon2 = { workspace = true, features = ["zeroize"] }
+axum = { workspace = true, optional = true }
 futures-util.workspace = true
 push-domain.workspace = true
 push-proto.workspace = true
```

Create `crates/push-sdk/src/bin/push-sdk-fakes.rs`:

```rust
//! A fake auth-svc and a gateway that answers by itself (`push_sdk::testing`), for the
//! bindings' tests run outside Rust (Node's): prints their URLs and the identity they
//! issue as one JSON line, then serves until its standard input closes.

use std::io::Write;

use push_sdk::testing::{AutoGateway, FakeAuth};

#[tokio::main]
async fn main() {
    let auth = FakeAuth::start().await;
    let gateway = AutoGateway::start(auth.identity.clone()).await;
    let line = serde_json::json!({
        "authUrl": auth.url,
        "socketUrl": gateway.url,
        "userId": auth.user.to_string(),
        "deviceId": auth.device.to_string(),
    });
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
    let _ =
        tokio::task::spawn_blocking(|| std::io::copy(&mut std::io::stdin(), &mut std::io::sink()))
            .await;
}
```

Change `crates/push-sdk/src/client.rs` (save the block to a file and `git apply` it):

```diff
diff --git a/crates/push-sdk/src/client.rs b/crates/push-sdk/src/client.rs
index 991145a..829d191 100644
--- a/crates/push-sdk/src/client.rs
+++ b/crates/push-sdk/src/client.rs
@@ -246,6 +246,16 @@ impl Client {
         runtime.spawn(future).await.unwrap_or(Err(SdkError::Closed))
     }
 
+    /// Starts `future` on the client's runtime and returns at once: for the bindings,
+    /// which settle a promise or call back when it ends, so they need no runtime of their
+    /// own (spec §8.5). A future still running when the client closes is dropped
+    /// unfinished. [`SdkError::Closed`] once the client is closed.
+    pub fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) -> Result<(), SdkError> {
+        let runtime = self.inner.runtime.as_ref().ok_or(SdkError::Closed)?;
+        drop(runtime.spawn(future));
+        Ok(())
+    }
+
     fn command(&self, command: Command) -> Result<(), SdkError> {
         self.inner
             .commands
@@ -905,6 +915,74 @@ mod tests {
         assert_eq!(auth.revocations.load(Ordering::SeqCst), 1);
     }
 
+    #[tokio::test]
+    async fn spawned_work_runs_on_the_client_runtime() {
+        let f = Fixture::new().await;
+        let (done, ran) = oneshot::channel();
+        f.client
+            .spawn(async move {
+                let _ = done.send(std::thread::current().name().map(str::to_owned));
+            })
+            .unwrap();
+        assert_eq!(ran.await.unwrap().as_deref(), Some("push-sdk"));
+    }
+
+    /// The bindings' tests talk to these doubles from outside Rust.
+    #[tokio::test]
+    async fn the_automatic_gateway_stores_sends_and_answers_them() {
+        let auth = FakeAuth::start().await;
+        let gateway = crate::testing::AutoGateway::start(auth.identity.clone()).await;
+        let database = format!("file:client-{}?mode=memory&cache=shared", Uuid::now_v7());
+        let config = Config::new(&auth.url, &gateway.url, &database);
+        let (client, mut events) =
+            Client::open(config, Arc::new(MemorySecretStore::new())).unwrap();
+        let wrong = client
+            .sign_in(
+                "a@example.com",
+                crate::testing::WRONG_PASSWORD,
+                "phone",
+                Platform::Web,
+            )
+            .await
+            .unwrap_err();
+        assert_eq!(wrong.code(), "invalid_credentials");
+        client
+            .sign_in("a@example.com", "password", "phone", Platform::Web)
+            .await
+            .unwrap();
+        client.connect().unwrap();
+        let conversation = client.open_direct(UserId::new()).await.unwrap();
+        let sent = client.send(conversation.id, "hello").unwrap();
+        let (mut stored, mut answered) = (None, None);
+        while stored.is_none() || answered.is_none() {
+            let event = tokio::time::timeout(Duration::from_secs(10), events.next())
+                .await
+                .expect("an event within 10 s")
+                .unwrap();
+            match event {
+                Event::MessageSent(message) if message.client_msg_id == sent.client_msg_id => {
+                    stored = message.seq;
+                }
+                Event::MessageAdded(message) if message.sender_id != auth.user => {
+                    answered = Some((message.seq, message.body));
+                }
+                _ => {}
+            }
+        }
+        assert_eq!(stored, Some(1));
+        assert_eq!(answered, Some((Some(2), Some("hello".to_owned()))));
+        let unknown = client
+            .redeem_invite(crate::testing::UNKNOWN_INVITE)
+            .await
+            .unwrap_err();
+        assert_eq!(unknown.code(), "invite_not_found");
+        assert_eq!(client.list_contacts().await, Ok(Vec::new()));
+        assert_eq!(
+            client.revoke_invite("ABCDEFGHJK").await.unwrap_err().code(),
+            "rejected"
+        );
+    }
+
     #[tokio::test]
     async fn an_encrypted_database_opens_only_with_its_key() {
         let scratch = crate::store::tests::Scratch::new();
```

Change `crates/push-sdk/src/key.rs` (save the block to a file and `git apply` it):

```diff
diff --git a/crates/push-sdk/src/key.rs b/crates/push-sdk/src/key.rs
index 5d42943..15f57c2 100644
--- a/crates/push-sdk/src/key.rs
+++ b/crates/push-sdk/src/key.rs
@@ -57,6 +57,12 @@ impl DatabaseKey {
         Ok(Self { bytes })
     }
 
+    /// The key's 32 bytes, for an app that keeps a derived key itself (in a hardware
+    /// keystore, say) and for the bindings, which hand it to the app. Never log them.
+    pub fn as_bytes(&self) -> &[u8; Self::LEN] {
+        &self.bytes
+    }
+
     /// A new random salt for [`DatabaseKey::derive`].
     pub fn new_salt() -> [u8; Self::SALT_LEN] {
         let mut salt = [0; Self::SALT_LEN];
@@ -105,6 +111,7 @@ mod tests {
         assert_eq!(format!("{key:?}"), "DatabaseKey(<redacted>)");
         assert!(DatabaseKey::derive(b"1234", &[0; 8]).is_err());
         assert!(DatabaseKey::from_bytes(&[1; 31]).is_err());
+        assert_eq!(DatabaseKey::from_bytes(key.as_bytes()).unwrap(), key);
         assert_eq!(
             DatabaseKey::from_bytes(&[0xab; 32]).unwrap().hex().as_str(),
             "ab".repeat(32)
```

Change `crates/push-sdk/src/lib.rs` (save the block to a file and `git apply` it):

```diff
diff --git a/crates/push-sdk/src/lib.rs b/crates/push-sdk/src/lib.rs
index 701de57..4aaaf29 100644
--- a/crates/push-sdk/src/lib.rs
+++ b/crates/push-sdk/src/lib.rs
@@ -21,8 +21,9 @@ mod model;
 mod policy;
 mod secrets;
 mod store;
-#[cfg(test)]
-mod testing;
+#[cfg(any(test, feature = "testing"))]
+#[doc(hidden)]
+pub mod testing;
 
 pub use auth::{Device, Platform, Profile};
 pub use client::{Account, Client, Config, EDIT_WINDOW, Events};
```

Change `crates/push-sdk/src/testing.rs` (save the block to a file and `git apply` it):

```diff
diff --git a/crates/push-sdk/src/testing.rs b/crates/push-sdk/src/testing.rs
index 077404f..ad662cb 100644
--- a/crates/push-sdk/src/testing.rs
+++ b/crates/push-sdk/src/testing.rs
@@ -1,6 +1,8 @@
-//! Test doubles shared by the connection's and the client's tests: a gateway driven frame
-//! by frame, and auth-svc answering with counters.
+//! Test doubles: a gateway driven frame by frame and auth-svc answering with counters, for
+//! the SDK's own tests; and [`AutoGateway`], a gateway that answers by itself, for the
+//! bindings' tests (feature `testing`, never in an app).
 
+use std::collections::HashMap;
 use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
 use std::sync::{Arc, Mutex};
 use std::time::Duration;
@@ -11,8 +13,12 @@ use axum::http::StatusCode;
 use axum::response::IntoResponse;
 use axum::routing::{delete, post};
 use futures_util::{SinkExt, StreamExt};
-use push_domain::{DeviceId, UserId};
+use push_domain::{ConversationId, DeviceId, UserId};
 use push_proto::v1::frame::Body;
+use push_proto::v1::{
+    ContactList, ContentType, ConversationList, Deliver, DirectOpened, EntryKind, ErrorCode,
+    InviteRedeemed, Pong, Ready, Request, Response, SendAck, request, response,
+};
 use push_proto::{decode_frame, encode_frame};
 use serde_json::{Value, json};
 use tokio::net::TcpStream;
@@ -22,11 +28,12 @@ use tokio_tungstenite::tungstenite::Message;
 use tokio_tungstenite::tungstenite::protocol::CloseFrame;
 use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
 use tokio_tungstenite::{WebSocketStream, accept_async};
+use uuid::Uuid;
 
-pub(crate) type Server = WebSocketStream<TcpStream>;
+pub type Server = WebSocketStream<TcpStream>;
 
 /// A gateway the test drives frame by frame: each accepted connection is handed over.
-pub(crate) struct FakeGateway {
+pub struct FakeGateway {
     pub url: String,
     connections: mpsc::UnboundedReceiver<Server>,
 }
@@ -60,7 +67,7 @@ impl FakeGateway {
 }
 
 /// The next frame from the client.
-pub(crate) async fn recv(socket: &mut Server) -> Body {
+pub async fn recv(socket: &mut Server) -> Body {
     loop {
         let message = timeout(Duration::from_secs(10), socket.next())
             .await
@@ -74,7 +81,7 @@ pub(crate) async fn recv(socket: &mut Server) -> Body {
 }
 
 /// The next frame `wanted` accepts, skipping the others.
-pub(crate) async fn recv_until(socket: &mut Server, wanted: impl Fn(&Body) -> bool) -> Body {
+pub async fn recv_until(socket: &mut Server, wanted: impl Fn(&Body) -> bool) -> Body {
     loop {
         let body = recv(socket).await;
         if wanted(&body) {
@@ -83,7 +90,7 @@ pub(crate) async fn recv_until(socket: &mut Server, wanted: impl Fn(&Body) -> bo
     }
 }
 
-pub(crate) async fn send(socket: &mut Server, body: Body) {
+pub async fn send(socket: &mut Server, body: Body) {
     socket
         .send(Message::Binary(encode_frame(body).into()))
         .await
@@ -91,14 +98,14 @@ pub(crate) async fn send(socket: &mut Server, body: Body) {
 }
 
 /// Like [`send`], but false instead of a panic once the connection is gone.
-pub(crate) async fn try_send(socket: &mut Server, body: Body) -> bool {
+pub async fn try_send(socket: &mut Server, body: Body) -> bool {
     socket
         .send(Message::Binary(encode_frame(body).into()))
         .await
         .is_ok()
 }
 
-pub(crate) async fn close(socket: &mut Server, code: u16) {
+pub async fn close(socket: &mut Server, code: u16) {
     let frame = CloseFrame {
         code: CloseCode::from(code),
         reason: "".into(),
@@ -108,7 +115,7 @@ pub(crate) async fn close(socket: &mut Server, code: u16) {
 
 /// auth-svc for one user and device: sign-in, registration, refresh (answering
 /// `access-N`) and revocation, each counted.
-pub(crate) struct FakeAuth {
+pub struct FakeAuth {
     pub url: String,
     pub user: UserId,
     pub device: DeviceId,
@@ -126,8 +133,13 @@ pub(crate) struct FakeAuth {
     pub spend_next_registration: Arc<AtomicBool>,
     /// The `registration_id` of every registration, in order.
     pub registration_ids: Arc<Mutex<Vec<String>>>,
+    /// Sign-ups answered.
+    pub signups: Arc<AtomicUsize>,
 }
 
+/// The password [`FakeAuth`] refuses with `invalid_credentials`; it accepts any other.
+pub const WRONG_PASSWORD: &str = "wrong password";
+
 impl FakeAuth {
     pub async fn start() -> Self {
         let (user, device) = (UserId::new(), DeviceId::new());
@@ -139,6 +151,8 @@ impl FakeAuth {
         let fail_revocations = Arc::new(AtomicBool::new(false));
         let spend_next_registration = Arc::new(AtomicBool::new(false));
         let registration_ids = Arc::new(Mutex::new(Vec::new()));
+        let signups = Arc::new(AtomicUsize::new(0));
+        let signed_up = signups.clone();
         let identity = Arc::new(Mutex::new((user, device)));
         let current = identity.clone();
         let tokens = move |n: usize| {
@@ -171,6 +185,10 @@ impl FakeAuth {
                 post(move |Json(body): Json<Value>| {
                     let (logins, tokens) = (logins.clone(), login_tokens.clone());
                     async move {
+                        if body["password"] == WRONG_PASSWORD {
+                            let error = json!({"error": {"code": "invalid_credentials", "message": "wrong email or password", "retryable": false}});
+                            return (StatusCode::UNAUTHORIZED, Json(error)).into_response();
+                        }
                         if body.get("device_id").is_some() {
                             let n = logins.fetch_add(1, Ordering::SeqCst) + 100;
                             Json(tokens(n)).into_response()
@@ -223,7 +241,19 @@ impl FakeAuth {
                         Json(tokens(n))
                     }
                 }),
-            );
+            )
+            .route(
+                "/v1/auth/signup",
+                post(move || {
+                    let signed_up = signed_up.clone();
+                    async move {
+                        signed_up.fetch_add(1, Ordering::SeqCst);
+                        StatusCode::ACCEPTED
+                    }
+                }),
+            )
+            .route("/v1/auth/verify/resend", post(|| async { StatusCode::ACCEPTED }))
+            .route("/v1/auth/password/forgot", post(|| async { StatusCode::ACCEPTED }));
         let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
         let url = format!("http://{}", listener.local_addr().unwrap());
         tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
@@ -240,6 +270,198 @@ impl FakeAuth {
             fail_revocations,
             spend_next_registration,
             registration_ids,
+            signups,
         }
     }
 }
+
+/// The invite code [`AutoGateway`] answers `invite_not_found` for; it redeems any other.
+pub const UNKNOWN_INVITE: &str = "NOSUCHCODE";
+
+/// A gateway that answers by itself, for tests that drive the SDK from outside Rust (the
+/// bindings'): `Identify` gets `Ready` for the identity [`FakeAuth`] issues, `Ping` gets
+/// `Pong`, every send, edit and delete is stored with the next `seq` of its conversation,
+/// and every text message sent is answered by the peer with the same text, delivered next.
+/// `OpenDirect`, `CreateInvite`, `RedeemInvite` and the listings are answered; any other
+/// request is refused with `rejected`.
+pub struct AutoGateway {
+    pub url: String,
+}
+
+#[derive(Default)]
+struct Log {
+    /// Each conversation's peer and last `seq`.
+    conversations: HashMap<ConversationId, (UserId, i64)>,
+}
+
+impl AutoGateway {
+    pub async fn start(identity: Arc<Mutex<(UserId, DeviceId)>>) -> Self {
+        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
+        let url = format!("ws://{}/v1/socket", listener.local_addr().unwrap());
+        let log = Arc::new(Mutex::new(Log::default()));
+        tokio::spawn(async move {
+            while let Ok((stream, _)) = listener.accept().await {
+                let (identity, log) = (identity.clone(), log.clone());
+                tokio::spawn(async move {
+                    if let Ok(socket) = accept_async(stream).await {
+                        serve(socket, &identity, &log).await;
+                    }
+                });
+            }
+        });
+        Self { url }
+    }
+}
+
+fn now_us() -> i64 {
+    let since = std::time::SystemTime::now()
+        .duration_since(std::time::UNIX_EPOCH)
+        .unwrap_or_default();
+    i64::try_from(since.as_micros()).unwrap_or(i64::MAX)
+}
+
+fn id_bytes(id: Uuid) -> Vec<u8> {
+    id.as_bytes().to_vec()
+}
+
+fn conversation_of(bytes: &[u8]) -> ConversationId {
+    ConversationId(Uuid::from_slice(bytes).unwrap_or_default())
+}
+
+/// The next `seq` of `conversation`, and its peer (a stranger for one never opened).
+fn append(log: &Mutex<Log>, conversation: ConversationId) -> (i64, UserId) {
+    let mut log = log.lock().unwrap();
+    let (peer, seq) = log
+        .conversations
+        .entry(conversation)
+        .or_insert_with(|| (UserId::new(), 0));
+    *seq += 1;
+    (*seq, *peer)
+}
+
+fn ack(client_msg_id: Vec<u8>, seq: i64) -> Body {
+    Body::SendAck(SendAck {
+        client_msg_id,
+        message_id: id_bytes(Uuid::now_v7()),
+        seq,
+        server_ts: now_us(),
+    })
+}
+
+async fn serve(mut socket: Server, identity: &Mutex<(UserId, DeviceId)>, log: &Mutex<Log>) {
+    while let Some(Ok(message)) = socket.next().await {
+        let Message::Binary(bytes) = message else {
+            continue;
+        };
+        let Ok(body) = decode_frame(&bytes) else {
+            return;
+        };
+        let answers = match body {
+            Body::Identify(_) => {
+                let (user, device) = *identity.lock().unwrap();
+                vec![Body::Ready(Ready {
+                    user_id: id_bytes(user.0),
+                    device_id: id_bytes(device.0),
+                    heartbeat_ms: 30_000,
+                })]
+            }
+            Body::Ping(_) => vec![Body::Pong(Pong {})],
+            Body::Send(send) => {
+                let conversation = conversation_of(&send.conversation_id);
+                let (seq, _) = append(log, conversation);
+                let (reply_seq, peer) = append(log, conversation);
+                let reply = Deliver {
+                    conversation_id: send.conversation_id,
+                    seq: reply_seq,
+                    message_id: id_bytes(Uuid::now_v7()),
+                    client_msg_id: id_bytes(Uuid::now_v7()),
+                    kind: EntryKind::Message.into(),
+                    target_seq: None,
+                    sender_id: id_bytes(peer.0),
+                    content_type: ContentType::Text.into(),
+                    body: Some(send.body),
+                    client_ts: None,
+                    server_ts: now_us(),
+                    edited_at: None,
+                    edit_seq: None,
+                    deleted: false,
+                };
+                vec![ack(send.client_msg_id, seq), Body::Deliver(reply)]
+            }
+            Body::Edit(edit) => {
+                let (seq, _) = append(log, conversation_of(&edit.conversation_id));
+                vec![ack(edit.client_msg_id, seq)]
+            }
+            Body::Delete(delete) => {
+                let (seq, _) = append(log, conversation_of(&delete.conversation_id));
+                vec![ack(delete.client_msg_id, seq)]
+            }
+            Body::Request(request) => vec![answer(request, log)],
+            _ => Vec::new(),
+        };
+        for answer in answers {
+            if socket
+                .send(Message::Binary(encode_frame(answer).into()))
+                .await
+                .is_err()
+            {
+                return;
+            }
+        }
+    }
+}
+
+fn answer(request: Request, log: &Mutex<Log>) -> Body {
+    let request_id = request.request_id;
+    let refuse = |code: ErrorCode, message: &str| {
+        Body::Error(push_proto::v1::Error {
+            code: code.into(),
+            message: message.to_owned(),
+            retryable: false,
+            request_id: Some(request_id),
+            ..Default::default()
+        })
+    };
+    let body = match request.body {
+        Some(request::Body::OpenDirect(open)) => {
+            let peer = UserId(Uuid::from_slice(&open.peer_user_id).unwrap_or_default());
+            let mut log = log.lock().unwrap();
+            let existing = log
+                .conversations
+                .iter()
+                .find(|(_, (known, _))| *known == peer)
+                .map(|(id, _)| *id);
+            let id = existing.unwrap_or_else(|| {
+                let id = ConversationId::new();
+                log.conversations.insert(id, (peer, 0));
+                id
+            });
+            response::Body::DirectOpened(DirectOpened {
+                conversation_id: id_bytes(id.0),
+            })
+        }
+        Some(request::Body::ListConversations(_)) => {
+            response::Body::Conversations(ConversationList::default())
+        }
+        Some(request::Body::ListContacts(_)) => response::Body::Contacts(ContactList::default()),
+        Some(request::Body::CreateInvite(create)) => {
+            response::Body::Invite(push_proto::v1::Invite {
+                code: "ABCDEFGHJK".to_owned(),
+                expires_at: now_us() + i64::from(create.expires_in_s) * 1_000_000,
+                max_uses: create.max_uses,
+                uses: 0,
+            })
+        }
+        Some(request::Body::RedeemInvite(redeem)) if redeem.code == UNKNOWN_INVITE => {
+            return refuse(ErrorCode::InviteNotFound, "no such invite");
+        }
+        Some(request::Body::RedeemInvite(_)) => response::Body::InviteRedeemed(InviteRedeemed {
+            user_id: id_bytes(Uuid::now_v7()),
+        }),
+        _ => return refuse(ErrorCode::Rejected, "the fake gateway does not answer this"),
+    };
+    Body::Response(Response {
+        request_id,
+        body: Some(body),
+    })
+}
```

- [ ] **Step 2: Red — a spawn that never runs fails its test**

In `crates/push-sdk/src/client.rs`, `Client::spawn`, replace `drop(runtime.spawn(future));`
with `drop(future);` and run:

```powershell
cargo test -p push-sdk --lib --locked client::tests::spawned_work_runs_on_the_client_runtime
```

Expected: `test client::tests::spawned_work_runs_on_the_client_runtime ... FAILED` (the
future was dropped unrun, so its channel reports `RecvError`). Restore the line.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test -p push-sdk --features testing --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p push-sdk --all-targets --features testing --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A crates
git commit -m "push-sdk: Client::spawn for the bindings; the test doubles behind a testing feature, with a gateway that answers by itself"
```

Expected: 968 tests pass in the workspace, no warnings, no diff. (No workspace crate
enables `testing` yet, so the first command is the one that builds `push-sdk-fakes`.)

---

### Task 2: What the bindings share: the JSON shapes and the wait

**Files:**
- Create: `bindings/ffi/Cargo.toml`, `bindings/ffi/src/lib.rs`, `bindings/ffi/src/json.rs`
- Modify: `Cargo.toml`, `Cargo.lock`

Every type, event and error as JSON (camelCase fields and values, the SDK's snake_case
error codes, ids as UUID strings, times in microseconds), the config with the key apart
as bytes (an unknown field refused), and `block_on`.

- [ ] **Step 1: Write the code**

Change `Cargo.lock` (save the block to a file and `git apply` it):

```diff
diff --git a/Cargo.lock b/Cargo.lock
index f43d6a2..b5b9084 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -2228,6 +2228,16 @@ dependencies = [
  "zeroize",
 ]
 
+[[package]]
+name = "push-sdk-ffi"
+version = "0.1.0"
+dependencies = [
+ "push-sdk",
+ "serde",
+ "serde_json",
+ "tokio",
+]
+
 [[package]]
 name = "push-smoke"
 version = "0.1.0"
```

Change `Cargo.toml` (save the block to a file and `git apply` it):

```diff
diff --git a/Cargo.toml b/Cargo.toml
index eec66c8..ec148f0 100644
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -1,6 +1,7 @@
 [workspace]
 resolver = "3"
 members = [
+    "bindings/ffi",
     "crates/push-auth",
     "crates/push-domain",
     "crates/push-email",
@@ -42,6 +43,7 @@ push-gateway = { path = "crates/push-gateway" }
 push-message-svc = { path = "crates/push-message-svc" }
 push-proto = { path = "crates/push-proto" }
 push-sdk = { path = "crates/push-sdk" }
+push-sdk-ffi = { path = "bindings/ffi" }
 push-store = { path = "crates/push-store" }
 push-testkit = { path = "crates/push-testkit" }
 push-worker = { path = "crates/push-worker" }
```

Create `bindings/ffi/Cargo.toml`:

```toml
[package]
name = "push-sdk-ffi"
description = "What push-sdk's Node and C bindings share: the data and error codes they hand to apps, as JSON, and waiting on the SDK from their own threads."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
publish = false

[dependencies]
push-sdk.workspace = true
serde.workspace = true
serde_json.workspace = true
# Only to tell the SDK's runtime threads apart.
tokio = { workspace = true, features = ["rt"] }

[dev-dependencies]
push-sdk = { workspace = true, features = ["testing"] }
tokio = { workspace = true, features = ["rt-multi-thread"] }

[lints]
workspace = true
```

Create `bindings/ffi/src/json.rs`:

```rust
//! The data push-sdk's bindings hand to apps (spec §8.5), as JSON, in one place: the Node
//! binding gives these values to JavaScript as objects and the C binding as UTF-8 strings,
//! so both show the same fields and, above all, the same error codes as
//! [`SdkError::code`].
//!
//! Field names and enum values are camelCase (`clientMsgId`, `"requestedOut"`); error
//! codes keep the SDK's snake_case (`"wrong_key"`). Ids are UUID strings; times are
//! microseconds since the Unix epoch, as in the SDK (they stay below 2^53, so JavaScript
//! numbers hold them exactly). The schema is documented in `docs/sdk-bindings.md`.
//!
//! Nothing here logs. The database key never passes through JSON: the bindings take it as
//! bytes ([`config`]).

use std::str::FromStr;
use std::time::Duration;

use push_sdk::{
    Account, ConnectionState, Contact, ContactState, Conversation, ConversationKind, DatabaseKey,
    Device, Event, Invite, Message, MessageKind, MessageStatus, OutboxKind, Platform, Profile,
    SdkError,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// An error: `{"code", "message", "retryable", "retryAfterS"}`. `code` is
/// [`SdkError::code`], identical in Rust, Node and C.
pub fn error(error: &SdkError) -> Value {
    let retry_after_s = match error {
        SdkError::Server { retry_after_s, .. } => *retry_after_s,
        _ => None,
    };
    json!({
        "code": error.code(),
        "message": error.to_string(),
        "retryable": error.is_retryable(),
        "retryAfterS": retry_after_s,
    })
}

/// `{"state": "offline" | "connecting" | "connected" | "reconnecting" | "signedOut" |
/// "deviceRemoved" | "openedElsewhere"}`, with `attempt` and `retryInMs` while
/// reconnecting.
pub fn connection_state(state: ConnectionState) -> Value {
    let name = match state {
        ConnectionState::Offline => "offline",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Connected => "connected",
        ConnectionState::Reconnecting {
            attempt,
            retry_in_ms,
        } => {
            return json!({
                "state": "reconnecting",
                "attempt": attempt,
                "retryInMs": retry_in_ms,
            });
        }
        ConnectionState::SignedOut => "signedOut",
        ConnectionState::DeviceRemoved => "deviceRemoved",
        ConnectionState::OpenedElsewhere => "openedElsewhere",
    };
    json!({ "state": name })
}

pub fn account(account: &Account) -> Value {
    json!({
        "userId": account.user_id.to_string(),
        "deviceId": account.device_id.to_string(),
    })
}

pub fn message(message: &Message) -> Value {
    let kind = match message.kind {
        MessageKind::Text => "text",
        MessageKind::System => "system",
    };
    let status = match message.status {
        MessageStatus::Pending => "pending",
        MessageStatus::Sent => "sent",
        MessageStatus::Delivered => "delivered",
        MessageStatus::Read => "read",
        MessageStatus::Received => "received",
    };
    json!({
        "conversationId": message.conversation_id.to_string(),
        "clientMsgId": message.client_msg_id.to_string(),
        "messageId": message.message_id.map(|id| id.to_string()),
        "seq": message.seq,
        "senderId": message.sender_id.to_string(),
        "kind": kind,
        "body": message.body,
        "clientTs": message.client_ts,
        "serverTs": message.server_ts,
        "editedAt": message.edited_at,
        "deleted": message.deleted,
        "status": status,
    })
}

pub fn conversation(conversation: &Conversation) -> Value {
    let kind = match conversation.kind {
        ConversationKind::Direct => "direct",
        ConversationKind::Group => "group",
        ConversationKind::Unknown => "unknown",
    };
    json!({
        "id": conversation.id.to_string(),
        "kind": kind,
        "peer": conversation.peer.map(|peer| peer.to_string()),
        "title": conversation.title,
        "lastSeq": conversation.last_seq,
        "readSeq": conversation.read_seq,
        "unread": conversation.unread,
        "peerDeliveredSeq": conversation.peer_delivered_seq,
        "peerReadSeq": conversation.peer_read_seq,
        "updatedAt": conversation.updated_at,
    })
}

pub fn contact(contact: &Contact) -> Value {
    let state = match contact.state {
        ContactState::None => "none",
        ContactState::RequestedOut => "requestedOut",
        ContactState::RequestedIn => "requestedIn",
        ContactState::Accepted => "accepted",
        ContactState::Blocked => "blocked",
    };
    json!({ "userId": contact.user_id.to_string(), "state": state })
}

pub fn invite(invite: &Invite) -> Value {
    json!({
        "code": invite.code,
        "expiresAt": invite.expires_at,
        "maxUses": invite.max_uses,
        "uses": invite.uses,
    })
}

pub fn profile(profile: &Profile) -> Value {
    json!({
        "userId": profile.user_id.to_string(),
        "handle": profile.handle,
        "displayName": profile.display_name,
    })
}

pub fn device(device: &Device) -> Value {
    json!({
        "deviceId": device.device_id.to_string(),
        "name": device.name,
        "platform": device.platform,
        "createdAt": device.created_at,
        "lastSeenAt": device.last_seen_at,
        "current": device.current,
    })
}

/// Each of `items` with `shape`, as an array.
pub fn list<T>(items: &[T], shape: impl Fn(&T) -> Value) -> Value {
    Value::Array(items.iter().map(shape).collect())
}

/// An event: `{"type": …}` and its fields (see `docs/sdk-bindings.md`).
pub fn event(event: &Event) -> Value {
    match event {
        Event::ConnectionStateChanged(state) => {
            let mut value = connection_state(*state);
            value["type"] = json!("connectionStateChanged");
            value
        }
        Event::MessageAdded(added) => json!({"type": "messageAdded", "message": message(added)}),
        Event::MessageSent(sent) => json!({"type": "messageSent", "message": message(sent)}),
        Event::MessageEdited(edited) => {
            json!({"type": "messageEdited", "message": message(edited)})
        }
        Event::MessageDeleted(deleted) => {
            json!({"type": "messageDeleted", "message": message(deleted)})
        }
        Event::SendFailed {
            conversation_id,
            client_msg_id,
            kind,
            error: failure,
        } => {
            let kind = match kind {
                OutboxKind::Send => "send",
                OutboxKind::Edit => "edit",
                OutboxKind::Delete => "delete",
            };
            json!({
                "type": "sendFailed",
                "conversationId": conversation_id.to_string(),
                "clientMsgId": client_msg_id.to_string(),
                "kind": kind,
                "error": error(failure),
            })
        }
        Event::ReceiptChanged {
            conversation_id,
            peer_delivered_seq,
            peer_read_seq,
        } => json!({
            "type": "receiptChanged",
            "conversationId": conversation_id.to_string(),
            "peerDeliveredSeq": peer_delivered_seq,
            "peerReadSeq": peer_read_seq,
        }),
        Event::ConversationChanged(changed) => {
            json!({"type": "conversationChanged", "conversation": conversation(changed)})
        }
        Event::ContactChanged(changed) => {
            json!({"type": "contactChanged", "contact": contact(changed)})
        }
        Event::SignedOut => json!({"type": "signedOut"}),
        Event::DeviceRemoved => json!({"type": "deviceRemoved"}),
        Event::OpenedElsewhere => json!({"type": "openedElsewhere"}),
        Event::EventsLost => json!({"type": "eventsLost"}),
    }
}

/// The settings an app opens a client with, as the bindings receive them.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigFields {
    auth_url: String,
    socket_url: String,
    database_path: String,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    request_timeout_ms: Option<u64>,
}

/// A [`push_sdk::Config`] from `{"authUrl", "socketUrl", "databasePath", "host"?,
/// "requestTimeoutMs"?}` and, apart from the JSON so it never sits in a string, the
/// database key's 32 bytes. An unknown field is refused, so a misspelt setting is not
/// silently dropped.
pub fn config(value: Value, database_key: Option<&[u8]>) -> Result<push_sdk::Config, SdkError> {
    let fields: ConfigFields = serde_json::from_value(value)
        .map_err(|error| SdkError::Invalid(format!("config: {error}")))?;
    let mut config =
        push_sdk::Config::new(&fields.auth_url, &fields.socket_url, &fields.database_path);
    config.host = fields.host.filter(|host| !host.is_empty());
    if let Some(timeout) = fields.request_timeout_ms {
        config.request_timeout = Duration::from_millis(timeout);
    }
    config.database_key = database_key.map(DatabaseKey::from_bytes).transpose()?;
    Ok(config)
}

/// `"ios"`, `"android"`, `"web"`, `"desktop"` or `"other"`.
pub fn platform(name: &str) -> Result<Platform, SdkError> {
    Ok(match name {
        "ios" => Platform::Ios,
        "android" => Platform::Android,
        "web" => Platform::Web,
        "desktop" => Platform::Desktop,
        "other" => Platform::Other,
        _ => return Err(SdkError::Invalid(format!("unknown platform {name:?}"))),
    })
}

/// An id from its UUID string; `invalid` naming `what` otherwise.
pub fn id<T: FromStr>(text: &str, what: &str) -> Result<T, SdkError> {
    text.parse()
        .map_err(|_| SdkError::Invalid(format!("{what} is not an id")))
}

#[cfg(test)]
mod tests {
    use push_sdk::{ClientMsgId, ConversationId, DeviceId, UserId};

    use super::*;

    #[test]
    fn errors_carry_the_sdk_codes_unchanged() {
        let cases = [
            SdkError::Timeout,
            SdkError::UnexpectedAnswer("x".to_owned()),
            SdkError::ConnectionBusy,
            SdkError::EncryptionUnavailable,
            SdkError::WrongKey,
            SdkError::Offline,
            SdkError::Invalid("empty".to_owned()),
            SdkError::Server {
                code: "rate_limited".to_owned(),
                message: "slow down".to_owned(),
                retryable: true,
                retry_after_s: Some(7),
            },
        ];
        for case in &cases {
            let shown = error(case);
            assert_eq!(shown["code"], case.code(), "{case:?}");
            assert_eq!(shown["retryable"], case.is_retryable(), "{case:?}");
        }
        assert_eq!(error(&cases[0])["code"], "timeout");
        assert_eq!(error(&cases[0])["retryable"], false);
        assert_eq!(error(&cases[1])["retryable"], false);
        assert_eq!(error(&cases[2])["retryable"], true);
        assert_eq!(error(&cases[3])["code"], "encryption_unavailable");
        assert_eq!(error(&cases[4])["code"], "wrong_key");
        assert_eq!(error(&cases[7])["retryAfterS"], 7);
        assert_eq!(error(&cases[0])["retryAfterS"], Value::Null);
    }

    #[test]
    fn events_have_a_type_and_camel_case_fields() {
        let state = event(&Event::ConnectionStateChanged(
            ConnectionState::Reconnecting {
                attempt: 2,
                retry_in_ms: 1500,
            },
        ));
        assert_eq!(
            state,
            json!({"type": "connectionStateChanged", "state": "reconnecting", "attempt": 2, "retryInMs": 1500})
        );
        let (conversation_id, client_msg_id) = (ConversationId::new(), ClientMsgId::new());
        let failed = event(&Event::SendFailed {
            conversation_id,
            client_msg_id,
            kind: OutboxKind::Edit,
            error: SdkError::Server {
                code: "not_contact".to_owned(),
                message: "not a contact".to_owned(),
                retryable: false,
                retry_after_s: None,
            },
        });
        assert_eq!(failed["type"], "sendFailed");
        assert_eq!(failed["clientMsgId"], client_msg_id.to_string());
        assert_eq!(failed["kind"], "edit");
        assert_eq!(failed["error"]["code"], "not_contact");
        assert_eq!(event(&Event::EventsLost), json!({"type": "eventsLost"}));
        let sender = UserId::new();
        let added = event(&Event::MessageAdded(Message {
            conversation_id,
            client_msg_id,
            message_id: None,
            seq: Some(4),
            sender_id: sender,
            kind: MessageKind::Text,
            body: Some("hi".to_owned()),
            client_ts: None,
            server_ts: Some(1_760_000_000_000_000),
            edited_at: None,
            deleted: false,
            status: MessageStatus::Received,
        }));
        assert_eq!(added["message"]["senderId"], sender.to_string());
        assert_eq!(added["message"]["serverTs"], 1_760_000_000_000_000_i64);
        assert_eq!(added["message"]["messageId"], Value::Null);
        assert_eq!(added["message"]["status"], "received");
        let account = account(&Account {
            user_id: sender,
            device_id: DeviceId::new(),
        });
        assert_eq!(account["userId"], sender.to_string());
    }

    #[test]
    fn a_config_takes_known_fields_and_the_key_as_bytes() {
        let value = json!({
            "authUrl": "http://127.0.0.1:1",
            "socketUrl": "ws://127.0.0.1:1/v1/socket",
            "databasePath": ":memory:",
            "host": "push.local",
            "requestTimeoutMs": 2500,
        });
        let opened = config(value.clone(), Some(&[7; 32])).unwrap();
        assert_eq!(opened.host.as_deref(), Some("push.local"));
        assert_eq!(opened.request_timeout, Duration::from_millis(2500));
        assert!(opened.database_key.is_some());
        assert_eq!(config(value.clone(), None).unwrap().database_key, None);
        assert_eq!(
            config(value.clone(), Some(&[7; 31])).unwrap_err().code(),
            "invalid"
        );
        let mut misspelt = value;
        misspelt["databaseKey"] = json!("00");
        assert_eq!(config(misspelt, None).unwrap_err().code(), "invalid");
        assert_eq!(platform("desktop").unwrap(), Platform::Desktop);
        assert_eq!(platform("toaster").unwrap_err().code(), "invalid");
        assert!(id::<UserId>("not an id", "userId").is_err());
    }
}
```

Create `bindings/ffi/src/lib.rs`:

```rust
//! What push-sdk's bindings share (spec §8.5): the data and error codes they hand to apps,
//! as JSON ([`json`]), and [`block_on`], which waits for the SDK from a thread of the
//! binding's own without a second runtime.
//!
//! The bindings add no logic of their own (spec §8.1): they convert and wait.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::Thread;

pub mod json;

/// Wakes the thread that waits in [`block_on`].
struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs `future` to the end on the calling thread, parking it while the future waits.
///
/// The client's futures do their work on the client's own runtime and only wait here for
/// the answer, so a binding needs no runtime of its own (spec §8.5: the core owns the only
/// one). Never call this on one of the SDK's threads ([`on_sdk_thread`]): a runtime worker
/// parked here could be the one the answer needs.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        std::thread::park();
    }
}

/// Whether the calling thread is one of the SDK's runtime threads (a worker, or a blocking
/// thread running store work): where the SDK calls the app's secret store from, and where
/// [`block_on`] must not wait.
pub fn on_sdk_thread() -> bool {
    tokio::runtime::Handle::try_current().is_ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use push_sdk::testing::{AutoGateway, FakeAuth};
    use push_sdk::{Client, Config, MemorySecretStore, Platform};

    use super::*;

    /// A plain thread waits for the client's answers; the client's runtime does the work.
    #[test]
    fn a_plain_thread_waits_for_the_client_without_a_runtime_of_its_own() {
        // The fakes' own runtime, apart from the client's.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (auth, gateway) = runtime.block_on(async {
            let auth = FakeAuth::start().await;
            let gateway = AutoGateway::start(auth.identity.clone()).await;
            (auth, gateway)
        });
        let config = Config::new(&auth.url, &gateway.url, ":memory:");
        let (client, mut events) =
            Client::open(config, Arc::new(MemorySecretStore::new())).unwrap();
        assert!(!on_sdk_thread());
        let account = block_on(client.sign_in("a@example.com", "pw", "phone", Platform::Web));
        assert_eq!(account.unwrap().user_id, auth.user);
        client.connect().unwrap();
        block_on(client.wait_connected(Duration::from_secs(10))).unwrap();
        let first = block_on(events.next()).unwrap();
        assert_eq!(json::event(&first)["type"], "connectionStateChanged");
        let inside = Arc::new(std::sync::Mutex::new(None));
        let seen = inside.clone();
        let (done, finished) = std::sync::mpsc::channel();
        client
            .spawn(async move {
                *seen.lock().unwrap() = Some(on_sdk_thread());
                let _ = done.send(());
            })
            .unwrap();
        finished.recv().unwrap();
        assert_eq!(*inside.lock().unwrap(), Some(true));
        block_on(client.close()).unwrap();
        drop(runtime);
    }
}
```

- [ ] **Step 2: Red — an error that claims to be retryable**

In `bindings/ffi/src/json.rs`, `error`, replace `"retryable": error.is_retryable(),` with
`"retryable": true,` and run:

```powershell
cargo test -p push-sdk-ffi --locked
```

Expected: FAILED, `errors_carry_the_sdk_codes_unchanged` (`timeout` must not be
retryable). Restore the line.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A Cargo.toml Cargo.lock bindings
git commit -m "push-sdk-ffi: what the bindings share: the data and error codes as JSON, and waiting from their own threads"
```

Expected: 972 tests pass, no warnings, no diff.

---

### Task 3: The C ABI: handles, errors, the store, events, the account and the connection

**Files:**
- Create: `bindings/c/Cargo.toml`, `bindings/c/cbindgen.toml`, `bindings/c/src/lib.rs`,
  `bindings/c/src/client.rs`, `bindings/c/tests/header.rs`,
  `bindings/c/include/push_sdk.h` (generated)
- Modify: `Cargo.toml`, `Cargo.lock`

The boundary (`guard`: `catch_unwind`, the error out-parameter, refusal from inside a
store function), the error object, string ownership, `push_client_open`/`_destroy`, the
secret store, events (callback and poll), the key helpers, and the account and connection
calls. The rest of the calls come in Task 4.

- [ ] **Step 1: Write the code**

Change `Cargo.lock` (save the block to a file and `git apply` it):

```diff
diff --git a/Cargo.lock b/Cargo.lock
index b5b9084..89394e8 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -396,6 +396,24 @@ version = "1.12.1"
 source = "registry+https://github.com/rust-lang/crates.io-index"
 checksum = "fc652a48c352aef3ea3aed32080501cf3ef6ed5da78602a020c991775b0aff04"
 
+[[package]]
+name = "cbindgen"
+version = "0.29.4"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "2ecb53484c9c167ba674026b656d8a27d7657a58e6066aa902bfb1a4aa00ae20"
+dependencies = [
+ "heck",
+ "indexmap 2.14.2",
+ "log",
+ "proc-macro2",
+ "quote",
+ "serde",
+ "serde_json",
+ "syn 2.0.119",
+ "tempfile",
+ "toml",
+]
+
 [[package]]
 name = "cc"
 version = "1.8.0"
@@ -2228,6 +2246,17 @@ dependencies = [
  "zeroize",
 ]
 
+[[package]]
+name = "push-sdk-c"
+version = "0.1.0"
+dependencies = [
+ "cbindgen",
+ "push-sdk",
+ "push-sdk-ffi",
+ "serde_json",
+ "tokio",
+]
+
 [[package]]
 name = "push-sdk-ffi"
 version = "0.1.0"
@@ -2885,6 +2914,15 @@ dependencies = [
  "syn 3.0.8",
 ]
 
+[[package]]
+name = "serde_spanned"
+version = "1.1.2"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "4e7523beb55eece201a2356bee0bbca0d1ab466c14c07703b2e0ee6d42cb0c2c"
+dependencies = [
+ "serde_core",
+]
+
 [[package]]
 name = "serde_urlencoded"
 version = "0.7.1"
@@ -3551,6 +3589,45 @@ dependencies = [
  "tokio",
 ]
 
+[[package]]
+name = "toml"
+version = "0.9.12+spec-1.1.0"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "cf92845e79fc2e2def6a5d828f0801e29a2f8acc037becc5ab08595c7d5e9863"
+dependencies = [
+ "indexmap 2.14.2",
+ "serde_core",
+ "serde_spanned",
+ "toml_datetime",
+ "toml_parser",
+ "toml_writer",
+ "winnow 0.7.15",
+]
+
+[[package]]
+name = "toml_datetime"
+version = "0.7.5+spec-1.1.0"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "92e1cfed4a3038bc5a127e35a2d360f145e1f4b971b551a2ba5fd7aedf7e1347"
+dependencies = [
+ "serde_core",
+]
+
+[[package]]
+name = "toml_parser"
+version = "1.1.5+spec-1.1.0"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "baa693a8032d7e1cada7d0041e96126df243179ff061456783ac7f12bda4744c"
+dependencies = [
+ "winnow 1.0.4",
+]
+
+[[package]]
+name = "toml_writer"
+version = "1.1.3+spec-1.1.0"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "06bdbd8cfc056b8d2e2e85f29b56a3bdbecb527cef81eb39e3e7b98af4652770"
+
 [[package]]
 name = "tonic"
 version = "0.13.1"
@@ -4293,6 +4370,18 @@ version = "0.52.6"
 source = "registry+https://github.com/rust-lang/crates.io-index"
 checksum = "589f6da84c646204747d1270a2a5661ea66ed1cced2631d546fdfb155959f9ec"
 
+[[package]]
+name = "winnow"
+version = "0.7.15"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "df79d97927682d2fd8adb29682d1140b343be4ac0f08fd68b7765d9c059d3945"
+
+[[package]]
+name = "winnow"
+version = "1.0.4"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "23b97319f7b8343df12cc98938e5c3eb436064524c8d2b4e30a1d3a36eecdf81"
+
 [[package]]
 name = "wit-bindgen"
 version = "0.46.0"
```

Change `Cargo.toml` (save the block to a file and `git apply` it):

```diff
diff --git a/Cargo.toml b/Cargo.toml
index ec148f0..040d3d0 100644
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -1,6 +1,7 @@
 [workspace]
 resolver = "3"
 members = [
+    "bindings/c",
     "bindings/ffi",
     "crates/push-auth",
     "crates/push-domain",
@@ -43,6 +44,7 @@ push-gateway = { path = "crates/push-gateway" }
 push-message-svc = { path = "crates/push-message-svc" }
 push-proto = { path = "crates/push-proto" }
 push-sdk = { path = "crates/push-sdk" }
+push-sdk-c = { path = "bindings/c" }
 push-sdk-ffi = { path = "bindings/ffi" }
 push-store = { path = "crates/push-store" }
 push-testkit = { path = "crates/push-testkit" }
```

Create `bindings/c/Cargo.toml`:

```toml
[package]
name = "push-sdk-c"
description = "The C binding of push-sdk: a stable C ABI with opaque handles, error codes and UTF-8 JSON (spec §8.5)."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
publish = false

[lib]
# The shared library apps link (push_sdk_c.dll, libpush_sdk_c.so, libpush_sdk_c.dylib);
# rlib for the Rust tests.
crate-type = ["cdylib", "rlib"]

[dependencies]
push-sdk.workspace = true
push-sdk-ffi.workspace = true
serde_json.workspace = true
tokio = { workspace = true, features = ["sync", "time"] }

[dev-dependencies]
# Generates include/push_sdk.h; tests/header.rs fails when the checked-in header is stale.
cbindgen = { version = "0.29", default-features = false }
push-sdk = { workspace = true, features = ["testing"] }
tokio = { workspace = true, features = ["rt-multi-thread"] }

[lints.rust]
# The C ABI needs unsafe; every unsafe operation is still marked inside unsafe functions.
unsafe_op_in_unsafe_fn = "deny"

[lints.clippy]
all = { level = "warn", priority = -1 }
# Every function has the same safety rules (valid pointers, UTF-8, ownership), stated once
# in the crate's documentation and the header.
missing_safety_doc = "allow"
```

Create `bindings/c/cbindgen.toml`:

```toml
# cbindgen settings for include/push_sdk.h (tests/header.rs regenerates and compares it).
language = "C"
cpp_compat = true
include_guard = "PUSH_SDK_H"
sys_includes = ["stdbool.h", "stddef.h", "stdint.h"]
no_includes = true
usize_is_size_t = true
style = "type"
documentation_style = "c99"
header = """
/*
 * push-sdk's C API (spec §8.5). Generated by cbindgen from bindings/c/src: do not edit.
 * Regenerate with: $env:PUSH_SDK_BLESS_HEADER = '1'; cargo test -p push-sdk-c --test header
 * (bash: PUSH_SDK_BLESS_HEADER=1 cargo test -p push-sdk-c --test header).
 *
 * Rules (docs/sdk-bindings.md has the whole guide):
 * - Strings in are UTF-8, NUL-terminated, borrowed for the call. Strings out are UTF-8 JSON
 *   or ids the caller owns and frees with push_string_free.
 * - A function that can fail returns PUSH_OK or PUSH_ERROR and, given a non-NULL
 *   `PushError **error`, stores an error the caller frees with push_error_free. Its code
 *   (push_error_code) is the SDK's stable code, the same as in Rust and Node.
 * - Calls that talk to the servers block the calling thread until they end (each has the
 *   SDK's own time limits): call them off the UI thread, never from a secret store callback.
 * - Events come to one callback on an SDK thread, or from push_client_next_event.
 */
"""

[export]
prefix = ""

[enum]
rename_variants = "ScreamingSnakeCase"

[fn]
args = "auto"
```

Create `bindings/c/src/client.rs`:

```rust
//! The client's calls (spec §8.2), one C function each: the account and the connection.

use std::ffi::c_char;
use std::time::Duration;

use push_sdk::DatabaseKey;
use push_sdk_ffi::json;

use crate::{PushClient, PushError, bytes, guard, handle, place, put_json, text, wait};

// ----- account -----

/// Signs up; auth-svc emails a verification link.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_sign_up(
    client: *const PushClient,
    email: *const c_char,
    password: *const c_char,
    handle_name: *const c_char,
    display_name: *const c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let (email, password) = unsafe { (text(email, "email")?, text(password, "password")?) };
        let (handle_name, display_name) = unsafe {
            (
                text(handle_name, "handle_name")?,
                text(display_name, "display_name")?,
            )
        };
        wait(
            this.client
                .sign_up(email, password, handle_name, display_name),
        )
    })
}

/// Sends the verification email again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_resend_verification(
    client: *const PushClient,
    email: *const c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let email = unsafe { text(email, "email") }?;
        wait(this.client.resend_verification(email))
    })
}

/// Emails a password-reset link.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_forgot_password(
    client: *const PushClient,
    email: *const c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let email = unsafe { text(email, "email") }?;
        wait(this.client.forgot_password(email))
    })
}

/// Signs in on this device (`platform`: `ios`, `android`, `web`, `desktop` or `other`);
/// `*account_json` gets `{"userId", "deviceId"}`. Then connect.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_sign_in(
    client: *const PushClient,
    email: *const c_char,
    password: *const c_char,
    device_name: *const c_char,
    platform: *const c_char,
    account_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let out = place(account_json, "account_json")?;
        let (email, password) = unsafe { (text(email, "email")?, text(password, "password")?) };
        let (device_name, platform) = unsafe {
            (
                text(device_name, "device_name")?,
                text(platform, "platform")?,
            )
        };
        let platform = json::platform(platform)?;
        let account = wait(this.client.sign_in(email, password, device_name, platform))?;
        put_json(out, &json::account(&account));
        Ok(())
    })
}

/// The signed-in account as `{"userId", "deviceId"}`, or `null`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_account(
    client: *const PushClient,
    account_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let out = place(account_json, "account_json")?;
        let account = this.client.account()?;
        put_json(
            out,
            &account
                .as_ref()
                .map_or(serde_json::Value::Null, json::account),
        );
        Ok(())
    })
}

/// Signs this device out for good: forgets the credentials, erases the local data and asks
/// the server to revoke the device (spec §8.5).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_sign_out(
    client: *const PushClient,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        wait(this.client.sign_out())
    })
}

// ----- connection -----

/// Connects, and keeps reconnecting until `push_client_disconnect`. Returns at once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_connect(
    client: *const PushClient,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || unsafe { handle(client) }?.client.connect())
}

/// Closes the connection and stops reconnecting. Returns at once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_disconnect(
    client: *const PushClient,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || unsafe { handle(client) }?.client.disconnect())
}

/// The connection's state as `{"state": …}` (`attempt` and `retryInMs` while
/// reconnecting).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_connection_state(
    client: *const PushClient,
    state_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let out = place(state_json, "state_json")?;
        put_json(out, &json::connection_state(this.client.connection_state()));
        Ok(())
    })
}

/// Waits until connected, up to `timeout_ms` (`offline` if not).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_wait_connected(
    client: *const PushClient,
    timeout_ms: u32,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let timeout = Duration::from_millis(u64::from(timeout_ms));
        wait(this.client.wait_connected(timeout))
    })
}

/// Encrypts the local database with a new key of `PUSH_DATABASE_KEY_LEN` bytes, or with
/// NULL decrypts it. The next open needs the new key.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_change_database_key(
    client: *const PushClient,
    database_key: *const u8,
    database_key_len: usize,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let this = unsafe { handle(client) }?;
        let key = unsafe { bytes(database_key, database_key_len) }
            .map(DatabaseKey::from_bytes)
            .transpose()?;
        wait(this.client.change_database_key(key.as_ref()))
    })
}
```

Create `bindings/c/src/lib.rs`:

```rust
//! push-sdk's C binding (spec §8.5): a stable C ABI over [`push_sdk::Client`].
//!
//! - **Handles.** A `PushClient` is opaque: `push_client_open` creates it and
//!   `push_client_destroy` ends it. A `PushError` is opaque too, read with its accessors and
//!   freed with `push_error_free`.
//! - **Errors.** Every function that can fail returns [`PUSH_OK`] or [`PUSH_ERROR`] and, if
//!   the caller passes a place for it, an error whose code is the SDK's stable code
//!   ([`push_sdk::SdkError::code`]), the same in Rust, Node and C. A panic is caught at the
//!   boundary and reported as `panic`.
//! - **Strings.** In: UTF-8, NUL-terminated, borrowed for the call. Out: UTF-8 (JSON for
//!   anything with fields, shaped by [`push_sdk_ffi::json`]) that the caller owns and frees
//!   with `push_string_free`. JSON keeps the ABI stable as the SDK grows: a new field is
//!   not a new struct layout.
//! - **Blocking.** A call that talks to the servers blocks the calling thread until it ends;
//!   the SDK's own time limits bound every one (requests: 15 s for the connection and 15 s
//!   for the answer). Callbacks and completions would hand the app's context pointer to
//!   SDK threads for every call; blocking keeps ownership on the caller's stack, and a C
//!   app already has its own threads. Blocking calls are refused on the SDK's threads.
//! - **Events** come to one callback with a context pointer on an SDK thread
//!   (`push_client_set_event_callback`), or are polled (`push_client_next_event`).
//! - **The secret store** is the app's, as C function pointers ([`PushSecretStore`]).

use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use push_sdk::{Client, DatabaseKey, Events, SdkError, SecretError, SecretStore};
use push_sdk_ffi::{block_on, json, on_sdk_thread};
use serde_json::Value;

mod client;

pub use client::*;

/// The call succeeded.
pub const PUSH_OK: i32 = 0;
/// The call failed; the error says why.
pub const PUSH_ERROR: i32 = -1;
/// A secret store's `load`: there is no value under the key.
pub const PUSH_NOT_FOUND: i32 = 1;
/// The length of a database key, in bytes.
pub const PUSH_DATABASE_KEY_LEN: usize = 32;
/// The length of a salt `push_new_salt` makes, in bytes.
pub const PUSH_SALT_LEN: usize = 16;

/// Why a call failed. Read it with `push_error_code`, `push_error_message`,
/// `push_error_retryable` and `push_error_retry_after_s`; free it with `push_error_free`.
pub struct PushError {
    code: CString,
    message: CString,
    retryable: bool,
    retry_after_s: i32,
}

impl PushError {
    fn new(error: &SdkError) -> Self {
        let shown = json::error(error);
        let text = |name: &str| {
            let value = shown[name].as_str().unwrap_or_default();
            CString::new(value.replace('\0', " ")).unwrap_or_default()
        };
        let retry_after_s = shown["retryAfterS"]
            .as_i64()
            .and_then(|seconds| i32::try_from(seconds).ok())
            .unwrap_or(-1);
        Self {
            code: text("code"),
            message: text("message"),
            retryable: shown["retryable"].as_bool().unwrap_or(false),
            retry_after_s,
        }
    }

    fn panic() -> Self {
        Self {
            code: c"panic".to_owned(),
            message: c"the SDK failed unexpectedly (a bug); destroy the client".to_owned(),
            retryable: false,
            retry_after_s: -1,
        }
    }
}

/// The app's secret store (spec §8.2), as C function pointers. The SDK calls them on its
/// own threads and on the threads that call it, so they must be thread-safe, and they must
/// not call the SDK (those calls fail). Every function gets `context` first.
///
/// - `load` stores a value the app allocated in `*value` and `*value_len` and returns
///   `PUSH_OK`, or returns `PUSH_NOT_FOUND`, or `PUSH_ERROR`. The SDK copies the value and
///   hands it back to `free_value`.
/// - `save` replaces the value under `key` (`value_len` bytes, borrowed for the call).
/// - `remove` deletes it; removing a missing key succeeds.
/// - `release`, if set, is called once when the SDK no longer needs the store: after
///   `push_client_destroy`, or when `push_client_open` fails.
///
/// The keys are those of `SECRET_KEYS` in push-sdk: `push.session`, `push.device` and
/// `push.registration`. Values are opaque bytes. `docs/sdk-secure-storage.md` shows how to
/// keep them in the platform's keychain.
#[repr(C)]
pub struct PushSecretStore {
    pub context: *mut c_void,
    pub load: Option<
        unsafe extern "C" fn(
            context: *mut c_void,
            key: *const c_char,
            value: *mut *mut u8,
            value_len: *mut usize,
        ) -> i32,
    >,
    pub save: Option<
        unsafe extern "C" fn(
            context: *mut c_void,
            key: *const c_char,
            value: *const u8,
            value_len: usize,
        ) -> i32,
    >,
    pub remove: Option<unsafe extern "C" fn(context: *mut c_void, key: *const c_char) -> i32>,
    pub free_value:
        Option<unsafe extern "C" fn(context: *mut c_void, value: *mut u8, value_len: usize)>,
    pub release: Option<unsafe extern "C" fn(context: *mut c_void)>,
}

thread_local! {
    /// Set while this thread runs one of the app's secret store functions.
    static IN_STORE: Cell<bool> = const { Cell::new(false) };
}

/// The app's store, as the SDK sees it.
struct CStore(PushSecretStore);

// The app promises thread-safe functions and context (documented on PushSecretStore).
unsafe impl Send for CStore {}
unsafe impl Sync for CStore {}

impl CStore {
    /// Runs one of the app's functions, marked so that a call back into the SDK from it is
    /// refused.
    fn call<T>(&self, call: impl FnOnce() -> T) -> T {
        IN_STORE.with(|flag| flag.set(true));
        let result = call();
        IN_STORE.with(|flag| flag.set(false));
        result
    }

    fn key(key: &str) -> Result<CString, SecretError> {
        CString::new(key).map_err(|_| SecretError("a key holds a NUL byte".to_owned()))
    }
}

impl SecretStore for CStore {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError> {
        let load = self
            .0
            .load
            .ok_or_else(|| SecretError("no load function".to_owned()))?;
        let key = Self::key(key)?;
        let (mut value, mut value_len) = (ptr::null_mut::<u8>(), 0usize);
        let status =
            self.call(|| unsafe { load(self.0.context, key.as_ptr(), &mut value, &mut value_len) });
        match status {
            PUSH_NOT_FOUND => Ok(None),
            PUSH_OK => {
                let copied = if value.is_null() {
                    Vec::new()
                } else {
                    // The app gave `value_len` bytes at `value`, valid until free_value.
                    unsafe { std::slice::from_raw_parts(value, value_len) }.to_vec()
                };
                if let (Some(free_value), false) = (self.0.free_value, value.is_null()) {
                    self.call(|| unsafe {
                        free_value(self.0.context, value, value_len);
                    });
                }
                Ok(Some(copied))
            }
            _ => Err(SecretError(
                "the app's secret store could not load".to_owned(),
            )),
        }
    }

    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError> {
        let save = self
            .0
            .save
            .ok_or_else(|| SecretError("no save function".to_owned()))?;
        let key = Self::key(key)?;
        let status = self
            .call(|| unsafe { save(self.0.context, key.as_ptr(), value.as_ptr(), value.len()) });
        match status {
            PUSH_OK => Ok(()),
            _ => Err(SecretError(
                "the app's secret store could not save".to_owned(),
            )),
        }
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        let remove = self
            .0
            .remove
            .ok_or_else(|| SecretError("no remove function".to_owned()))?;
        let key = Self::key(key)?;
        let status = self.call(|| unsafe { remove(self.0.context, key.as_ptr()) });
        match status {
            PUSH_OK => Ok(()),
            _ => Err(SecretError(
                "the app's secret store could not remove".to_owned(),
            )),
        }
    }
}

impl Drop for CStore {
    fn drop(&mut self) {
        if let Some(release) = self.0.release {
            unsafe { release(self.0.context) };
        }
    }
}

/// Called with each event as UTF-8 JSON (`{"type": …}`), borrowed for the call, on an SDK
/// thread of its own, one event at a time and in order. It may call the SDK, including
/// blocking calls and `push_client_destroy`.
pub type PushEventCallback =
    Option<unsafe extern "C" fn(context: *mut c_void, event_json: *const c_char)>;

/// Called once, on the events thread, after the last event: the context may be freed.
pub type PushReleaseCallback = Option<unsafe extern "C" fn(context: *mut c_void)>;

/// How the app takes the events: not yet, by polling, or through a callback.
enum Delivery {
    Waiting(Events),
    Polled(Arc<tokio::sync::Mutex<Events>>),
    Called,
}

/// A client: opaque to C.
pub struct PushClient {
    client: Client,
    events: Mutex<Delivery>,
}

/// A pointer an SDK thread may carry: the app's context, which it promised is thread-safe.
struct Context(*mut c_void);

unsafe impl Send for Context {}

/// Runs `body` at the boundary: no panic crosses it, a failure becomes a `PushError` in
/// `*error` (when the app gave a place for it), and calls from inside a secret store
/// function are refused (they could wait for the store's own caller).
fn guard(error: *mut *mut PushError, body: impl FnOnce() -> Result<(), SdkError>) -> i32 {
    if !error.is_null() {
        // The app gave a place for an error: start it empty.
        unsafe { *error = ptr::null_mut() };
    }
    let outcome = if IN_STORE.with(Cell::get) {
        Ok(Err(SdkError::Invalid(
            "the SDK was called from a secret store function".to_owned(),
        )))
    } else {
        catch_unwind(AssertUnwindSafe(body))
    };
    let failure = match outcome {
        Ok(Ok(())) => return PUSH_OK,
        Ok(Err(sdk_error)) => PushError::new(&sdk_error),
        Err(_) => PushError::panic(),
    };
    if !error.is_null() {
        unsafe { *error = Box::into_raw(Box::new(failure)) };
    }
    PUSH_ERROR
}

/// Waits for `future` on the calling thread; refused on the SDK's own threads, where the
/// wait could hold up the answer.
fn wait<T>(future: impl Future<Output = Result<T, SdkError>>) -> Result<T, SdkError> {
    if on_sdk_thread() {
        return Err(SdkError::Invalid(
            "a blocking call on an SDK thread: call it from a thread of the app".to_owned(),
        ));
    }
    block_on(future)
}

/// A string argument, borrowed for the call.
unsafe fn text<'a>(value: *const c_char, what: &str) -> Result<&'a str, SdkError> {
    if value.is_null() {
        return Err(SdkError::Invalid(format!("{what} is missing")));
    }
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map_err(|_| SdkError::Invalid(format!("{what} is not UTF-8")))
}

/// Bytes an argument points to; none for NULL.
unsafe fn bytes<'a>(value: *const u8, len: usize) -> Option<&'a [u8]> {
    if value.is_null() {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(value, len) })
}

/// Checks a place for a result before any work is done, so a call never acts and then
/// fails to report.
fn place<T>(out: *mut T, what: &str) -> Result<*mut T, SdkError> {
    if out.is_null() {
        return Err(SdkError::Invalid(format!("{what} is NULL")));
    }
    Ok(out)
}

fn owned(value: String) -> *mut c_char {
    CString::new(value.replace('\0', " "))
        .unwrap_or_default()
        .into_raw()
}

/// Stores `value` as JSON in `*out`, for the caller to free with `push_string_free`.
fn put_json(out: *mut *mut c_char, value: &Value) {
    unsafe { *out = owned(value.to_string()) };
}

unsafe fn handle<'a>(client: *const PushClient) -> Result<&'a PushClient, SdkError> {
    if client.is_null() {
        return Err(SdkError::Invalid("the client is NULL".to_owned()));
    }
    Ok(unsafe { &*client })
}

/// The version of the SDK, as a static string (never freed).
#[unsafe(no_mangle)]
pub extern "C" fn push_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// The error's stable code (`timeout`, `wrong_key`, the server's `not_contact`, …),
/// valid until `push_error_free`. NULL for a NULL error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_error_code(error: *const PushError) -> *const c_char {
    match unsafe { error.as_ref() } {
        Some(error) => error.code.as_ptr(),
        None => ptr::null(),
    }
}

/// A sentence for logs, never containing a token, a key or a message's text; valid until
/// `push_error_free`. NULL for a NULL error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_error_message(error: *const PushError) -> *const c_char {
    match unsafe { error.as_ref() } {
        Some(error) => error.message.as_ptr(),
        None => ptr::null(),
    }
}

/// Whether the same call may succeed later, unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_error_retryable(error: *const PushError) -> bool {
    unsafe { error.as_ref() }.is_some_and(|error| error.retryable)
}

/// Rate limited: after how many seconds the same call may succeed; -1 if not said.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_error_retry_after_s(error: *const PushError) -> i32 {
    unsafe { error.as_ref() }.map_or(-1, |error| error.retry_after_s)
}

/// Frees an error; NULL is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_error_free(error: *mut PushError) {
    if !error.is_null() {
        drop(unsafe { Box::from_raw(error) });
    }
}

/// Frees a string the SDK returned; NULL is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_string_free(value: *mut c_char) {
    if !value.is_null() {
        drop(unsafe { CString::from_raw(value) });
    }
}

/// Opens a client: the local database (SQLite at `databasePath`) and the SDK's runtime.
/// It does not connect until `push_client_connect`. May take a second (a database key is
/// checked, the schema migrated): call it off the UI thread.
///
/// `config_json`: `{"authUrl", "socketUrl", "databasePath", "host"?, "requestTimeoutMs"?}`.
/// `database_key`: NULL for a plain database, or `PUSH_DATABASE_KEY_LEN` bytes that
/// encrypt the whole file (`wrong_key` if they do not open it); copied, never stored or
/// logged. `store`: the app's secret store, owned by the client from now on (released
/// even when this fails). `*client` gets the client.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_open(
    config_json: *const c_char,
    database_key: *const u8,
    database_key_len: usize,
    store: PushSecretStore,
    client: *mut *mut PushClient,
    error: *mut *mut PushError,
) -> i32 {
    // Owned first, so the store is released on every path.
    let store = CStore(store);
    guard(error, move || {
        let out = place(client, "client")?;
        let config = unsafe { text(config_json, "config_json") }?;
        let config: Value = serde_json::from_str(config)
            .map_err(|_| SdkError::Invalid("config_json is not JSON".to_owned()))?;
        let key = unsafe { bytes(database_key, database_key_len) };
        let config = json::config(config, key)?;
        let (opened, events) = Client::open(config, Arc::new(store))?;
        let handle = PushClient {
            client: opened,
            events: Mutex::new(Delivery::Waiting(events)),
        };
        unsafe { *out = Box::into_raw(Box::new(handle)) };
        Ok(())
    })
}

/// Closes the client and frees it: the connection stops, and the database is closed and
/// the secret store released before this returns (it waits up to a few seconds), so the
/// file can be opened again at once (also from the event callback). Called on one of the
/// SDK's runtime threads (from a secret store function) it does not wait. NULL is ignored.
/// No other call may use the client during or after this.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_destroy(client: *mut PushClient) {
    if client.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let handle = unsafe { Box::from_raw(client) };
        let PushClient { client, events } = *handle;
        drop(events);
        if on_sdk_thread() {
            drop(client);
        } else {
            let _ = block_on(client.close());
        }
    }));
}

/// Delivers every event to `callback` with `context`, on an SDK thread of its own, one at a
/// time and in order, until the client is destroyed; then `release` (if set) is called
/// once with `context`. Events wait in the SDK (at most 10,000; then one `eventsLost`)
/// until this is called. Only once per client, and not after `push_client_next_event`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_set_event_callback(
    client: *const PushClient,
    callback: PushEventCallback,
    context: *mut c_void,
    release: PushReleaseCallback,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let handle = unsafe { handle(client) }?;
        let callback =
            callback.ok_or_else(|| SdkError::Invalid("the callback is NULL".to_owned()))?;
        let mut delivery = handle.events.lock().map_err(|_| SdkError::Closed)?;
        let Delivery::Waiting(_) = &*delivery else {
            return Err(SdkError::Invalid("the events are taken already".to_owned()));
        };
        let Delivery::Waiting(mut events) = std::mem::replace(&mut *delivery, Delivery::Called)
        else {
            unreachable!("checked above");
        };
        let context = Context(context);
        std::thread::Builder::new()
            .name("push-sdk-events".to_owned())
            .spawn(move || {
                let context = context;
                while let Some(event) = block_on(events.next()) {
                    let shown = CString::new(json::event(&event).to_string()).unwrap_or_default();
                    unsafe { callback(context.0, shown.as_ptr()) };
                }
                if let Some(release) = release {
                    unsafe { release(context.0) };
                }
            })
            .map_err(|_| SdkError::Invalid("cannot start the events thread".to_owned()))?;
        Ok(())
    })
}

/// The next event as JSON in `*event_json` (the caller frees it), waiting up to
/// `timeout_ms`; NULL in `*event_json` if none came in time. Not after
/// `push_client_set_event_callback`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_client_next_event(
    client: *const PushClient,
    timeout_ms: u32,
    event_json: *mut *mut c_char,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let handle = unsafe { handle(client) }?;
        let out = place(event_json, "event_json")?;
        if on_sdk_thread() {
            return Err(SdkError::Invalid(
                "a blocking call on an SDK thread: call it from a thread of the app".to_owned(),
            ));
        }
        let events = {
            let mut delivery = handle.events.lock().map_err(|_| SdkError::Closed)?;
            if let Delivery::Waiting(_) = &*delivery {
                let Delivery::Waiting(events) = std::mem::replace(&mut *delivery, Delivery::Called)
                else {
                    unreachable!("checked above");
                };
                *delivery = Delivery::Polled(Arc::new(tokio::sync::Mutex::new(events)));
            }
            match &*delivery {
                Delivery::Polled(events) => events.clone(),
                _ => {
                    return Err(SdkError::Invalid("the events go to a callback".to_owned()));
                }
            }
        };
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        handle.client.spawn(async move {
            let mut events = events.lock().await;
            let next =
                tokio::time::timeout(Duration::from_millis(u64::from(timeout_ms)), events.next())
                    .await;
            let _ = sender.send(next);
        })?;
        match receiver.recv() {
            Ok(Ok(Some(event))) => put_json(out, &json::event(&event)),
            Ok(Err(_)) => unsafe { *out = ptr::null_mut() },
            Ok(Ok(None)) | Err(_) => return Err(SdkError::Closed),
        }
        Ok(())
    })
}

/// Derives a database key from what the user knows (a PIN, a passphrase) and a salt of at
/// least `PUSH_SALT_LEN` bytes the app keeps next to the database (Argon2id, 19 MiB, 2
/// passes): the same input always gives the same key. Writes `PUSH_DATABASE_KEY_LEN`
/// bytes to `key`. Takes tens of milliseconds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_derive_database_key(
    secret: *const u8,
    secret_len: usize,
    salt: *const u8,
    salt_len: usize,
    key: *mut u8,
    error: *mut *mut PushError,
) -> i32 {
    guard(error, || {
        let out = place(key, "key")?;
        let secret = unsafe { bytes(secret, secret_len) }
            .ok_or_else(|| SdkError::Invalid("the secret is NULL".to_owned()))?;
        let salt = unsafe { bytes(salt, salt_len) }
            .ok_or_else(|| SdkError::Invalid("the salt is NULL".to_owned()))?;
        let derived = DatabaseKey::derive(secret, salt)?;
        let derived = derived.as_bytes();
        unsafe { ptr::copy_nonoverlapping(derived.as_ptr(), out, PUSH_DATABASE_KEY_LEN) };
        Ok(())
    })
}

/// Writes a new random salt of `PUSH_SALT_LEN` bytes to `salt`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn push_new_salt(salt: *mut u8) -> i32 {
    guard(ptr::null_mut(), || {
        let out = place(salt, "salt")?;
        let made = DatabaseKey::new_salt();
        unsafe { ptr::copy_nonoverlapping(made.as_ptr(), out, PUSH_SALT_LEN) };
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

    use push_sdk::testing::{AutoGateway, FakeAuth};

    use super::*;

    fn code(error: *mut PushError) -> String {
        let code = unsafe { CStr::from_ptr(push_error_code(error)) }
            .to_str()
            .unwrap()
            .to_owned();
        unsafe { push_error_free(error) };
        code
    }

    #[test]
    fn a_panic_stops_at_the_boundary() {
        let mut error = ptr::null_mut();
        let status = guard(&mut error, || panic!("a bug"));
        assert_eq!(status, PUSH_ERROR);
        assert_eq!(code(error), "panic");
    }

    #[test]
    fn nulls_are_refused_before_anything_happens() {
        let mut error = ptr::null_mut();
        let status = unsafe { push_client_connect(ptr::null(), &mut error) };
        assert_eq!(status, PUSH_ERROR);
        assert_eq!(code(error), "invalid");
        // No place for the error: the status alone tells.
        assert_eq!(
            unsafe { push_client_connect(ptr::null(), ptr::null_mut()) },
            PUSH_ERROR
        );
        unsafe {
            push_string_free(ptr::null_mut());
            push_error_free(ptr::null_mut());
            push_client_destroy(ptr::null_mut());
        }
        assert!(unsafe { push_error_code(ptr::null()) }.is_null());
    }

    /// The client of the store's tests: set by the test before the store is called.
    static CLIENT: AtomicPtr<PushClient> = AtomicPtr::new(ptr::null_mut());
    static REFUSED: AtomicUsize = AtomicUsize::new(0);

    /// A store that calls back into the SDK from `save`, which the SDK must refuse rather
    /// than wait for itself.
    unsafe extern "C" fn reentrant_save(
        _context: *mut c_void,
        _key: *const c_char,
        _value: *const u8,
        _len: usize,
    ) -> i32 {
        let client = CLIENT.load(Ordering::SeqCst);
        let mut account = ptr::null_mut();
        let mut error = ptr::null_mut();
        if unsafe { push_client_account(client, &mut account, &mut error) } == PUSH_ERROR
            && code(error) == "invalid"
        {
            REFUSED.fetch_add(1, Ordering::SeqCst);
        }
        PUSH_OK
    }

    unsafe extern "C" fn empty_load(
        _context: *mut c_void,
        _key: *const c_char,
        _value: *mut *mut u8,
        _len: *mut usize,
    ) -> i32 {
        PUSH_NOT_FOUND
    }

    unsafe extern "C" fn removed(_context: *mut c_void, _key: *const c_char) -> i32 {
        PUSH_OK
    }

    #[test]
    fn the_sdk_refuses_calls_from_its_own_threads_and_from_the_store() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (auth, gateway) = runtime.block_on(async {
            let auth = FakeAuth::start().await;
            let gateway = AutoGateway::start(auth.identity.clone()).await;
            (auth, gateway)
        });
        let config = CString::new(format!(
            r#"{{"authUrl":"{}","socketUrl":"{}","databasePath":":memory:"}}"#,
            auth.url, gateway.url
        ))
        .unwrap();
        let store = PushSecretStore {
            context: ptr::null_mut(),
            load: Some(empty_load),
            save: Some(reentrant_save),
            remove: Some(removed),
            free_value: None,
            release: None,
        };
        let (mut client, mut error) = (ptr::null_mut(), ptr::null_mut());
        let opened = unsafe {
            push_client_open(
                config.as_ptr(),
                ptr::null(),
                0,
                store,
                &mut client,
                &mut error,
            )
        };
        assert_eq!(opened, PUSH_OK);
        CLIENT.store(client, Ordering::SeqCst);
        let mut account = ptr::null_mut();
        let signed_in = unsafe {
            push_client_sign_in(
                client,
                c"a@example.com".as_ptr(),
                c"pw".as_ptr(),
                c"phone".as_ptr(),
                c"web".as_ptr(),
                &mut account,
                &mut error,
            )
        };
        assert_eq!(signed_in, PUSH_OK);
        unsafe { push_string_free(account) };
        assert!(
            REFUSED.load(Ordering::SeqCst) >= 1,
            "the store's call came back refused"
        );

        // A blocking call on one of the SDK's threads is refused instead of waiting there.
        let (sender, receiver) = std::sync::mpsc::channel();
        let shared = CLIENT.load(Ordering::SeqCst) as usize;
        unsafe { &*client }
            .client
            .spawn(async move {
                let mut error = ptr::null_mut();
                let client = shared as *const PushClient;
                let status = unsafe {
                    push_client_forgot_password(client, c"a@example.com".as_ptr(), &mut error)
                };
                let _ = sender.send((status, code(error)));
            })
            .unwrap();
        assert_eq!(receiver.recv().unwrap(), (PUSH_ERROR, "invalid".to_owned()));
        unsafe { push_client_destroy(client) };
        drop(runtime);
    }
}
```

Create `bindings/c/tests/header.rs`:

```rust
//! `include/push_sdk.h` is generated from the crate by cbindgen and checked in, so C apps
//! need no Rust tooling; this test fails when it no longer matches the code. With
//! `PUSH_SDK_BLESS_HEADER=1` it writes the header instead.

use std::path::Path;

#[test]
fn the_checked_in_header_matches_the_code() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = cbindgen::Config::from_file(crate_dir.join("cbindgen.toml")).unwrap();
    let mut generated = Vec::new();
    cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_config(config)
        .generate()
        .expect("cbindgen reads the crate")
        .write(&mut generated);
    let generated = String::from_utf8(generated).unwrap();
    let path = crate_dir.join("include").join("push_sdk.h");
    if std::env::var_os("PUSH_SDK_BLESS_HEADER").is_some() {
        std::fs::write(&path, &generated).unwrap();
        return;
    }
    let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        checked_in == generated,
        "include/push_sdk.h is stale: regenerate it with PUSH_SDK_BLESS_HEADER=1 cargo test -p push-sdk-c --test header"
    );
}
```

Generate the header (its directory first) and check it:

```powershell
New-Item -ItemType Directory -Force bindings/c/include | Out-Null
$env:PUSH_SDK_BLESS_HEADER = '1'
cargo test -p push-sdk-c --test header --locked
Remove-Item Env:PUSH_SDK_BLESS_HEADER
cargo test -p push-sdk-c --test header --locked
(Get-FileHash bindings/c/include/push_sdk.h -Algorithm SHA256).Hash
```

Expected: both runs pass; the hash is `18154AD6979CF367FFCDCC7CD711158755A50E86CE56C1BED48138FE9E458BCB` (222 lines).

- [ ] **Step 2: Red — a store function that calls back in is not refused**

In `bindings/c/src/lib.rs`, `guard`, replace `let outcome = if IN_STORE.with(Cell::get) {`
with `let outcome = if false {` and run:

```powershell
cargo test -p push-sdk-c --lib --locked
```

Expected: FAILED, `the_sdk_refuses_calls_from_its_own_threads_and_from_the_store` ("the
store's call came back refused"). Restore the line.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A Cargo.toml Cargo.lock bindings
git commit -m "push-sdk-c: the C ABI: handles, errors, the secret store, events, keys, the account and the connection; a generated header"
```

Expected: 976 tests pass, no warnings, no diff.

---

### Task 4: The C calls for data, sending and requests; a C program

**Files:**
- Modify: `bindings/c/src/client.rs`, `bindings/c/Cargo.toml`, `Cargo.lock`,
  `bindings/c/include/push_sdk.h` (regenerated)
- Create: `bindings/c/build.rs`, `bindings/c/tests/c_program.rs`, `bindings/c/tests/c/flow.c`

- [ ] **Step 1: Write the code**

Change `Cargo.lock` (save the block to a file and `git apply` it):

```diff
diff --git a/Cargo.lock b/Cargo.lock
index 89394e8..7a07f20 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -2251,6 +2251,7 @@ name = "push-sdk-c"
 version = "0.1.0"
 dependencies = [
  "cbindgen",
+ "cc",
  "push-sdk",
  "push-sdk-ffi",
  "serde_json",
```

Change `bindings/c/Cargo.toml` (save the block to a file and `git apply` it):

```diff
diff --git a/bindings/c/Cargo.toml b/bindings/c/Cargo.toml
index 96d9924..b58cb50 100644
--- a/bindings/c/Cargo.toml
+++ b/bindings/c/Cargo.toml
@@ -22,6 +22,8 @@ tokio = { workspace = true, features = ["sync", "time"] }
 [dev-dependencies]
 # Generates include/push_sdk.h; tests/header.rs fails when the checked-in header is stale.
 cbindgen = { version = "0.29", default-features = false }
+# Compiles and links the C test program (MSVC on Windows, cc elsewhere).
+cc = "1"
 push-sdk = { workspace = true, features = ["testing"] }
 tokio = { workspace = true, features = ["rt-multi-thread"] }
 
```

Create `bindings/c/build.rs`:

```rust
fn main() {
    // tests/c_program.rs compiles a C program for the same target.
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    println!("cargo:rustc-env=PUSH_SDK_C_TARGET={target}");
}
```

Change `bindings/c/src/client.rs` (save the block to a file and `git apply` it):

```diff
diff --git a/bindings/c/src/client.rs b/bindings/c/src/client.rs
index 27f5000..c0c7cc0 100644
--- a/bindings/c/src/client.rs
+++ b/bindings/c/src/client.rs
@@ -1,12 +1,12 @@
-//! The client's calls (spec §8.2), one C function each: the account and the connection.
+//! The client's calls (spec §8.2), one C function each.
 
 use std::ffi::c_char;
 use std::time::Duration;
 
-use push_sdk::DatabaseKey;
+use push_sdk::{ClientMsgId, ConversationId, DatabaseKey, DeviceId, SdkError, UserId};
 use push_sdk_ffi::json;
 
-use crate::{PushClient, PushError, bytes, guard, handle, place, put_json, text, wait};
+use crate::{PushClient, PushError, bytes, guard, handle, owned, place, put_json, text, wait};
 
 // ----- account -----
 
@@ -194,3 +194,490 @@ pub unsafe extern "C" fn push_client_change_database_key(
         wait(this.client.change_database_key(key.as_ref()))
     })
 }
+
+// ----- local data -----
+
+fn conversation_id(value: *const c_char) -> Result<ConversationId, SdkError> {
+    json::id(
+        unsafe { text(value, "conversation_id") }?,
+        "conversation_id",
+    )
+}
+
+fn user_id(value: *const c_char) -> Result<UserId, SdkError> {
+    json::id(unsafe { text(value, "user_id") }?, "user_id")
+}
+
+fn client_msg_id(value: *const c_char, what: &str) -> Result<ClientMsgId, SdkError> {
+    json::id(unsafe { text(value, what) }?, what)
+}
+
+/// Every conversation this device knows, as a JSON array, most recently changed first.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_conversations(
+    client: *const PushClient,
+    conversations_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(conversations_json, "conversations_json")?;
+        let conversations = this.client.conversations()?;
+        put_json(out, &json::list(&conversations, json::conversation));
+        Ok(())
+    })
+}
+
+/// One conversation as JSON, or `null` if this device does not know it.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_conversation(
+    client: *const PushClient,
+    conversation_id_text: *const c_char,
+    conversation_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(conversation_json, "conversation_json")?;
+        let id = conversation_id(conversation_id_text)?;
+        let found = this.client.conversation(id)?;
+        put_json(
+            out,
+            &found
+                .as_ref()
+                .map_or(serde_json::Value::Null, json::conversation),
+        );
+        Ok(())
+    })
+}
+
+/// Up to `limit` stored messages before `*before_seq` (NULL: the newest, then those still
+/// in the outbox), oldest first, as a JSON array.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_messages(
+    client: *const PushClient,
+    conversation_id_text: *const c_char,
+    before_seq: *const i64,
+    limit: u32,
+    messages_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(messages_json, "messages_json")?;
+        let id = conversation_id(conversation_id_text)?;
+        let before = unsafe { before_seq.as_ref() }.copied();
+        let messages = this.client.messages(id, before, limit)?;
+        put_json(out, &json::list(&messages, json::message));
+        Ok(())
+    })
+}
+
+/// The user's relationships as this device knows them, as a JSON array.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_contacts(
+    client: *const PushClient,
+    contacts_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(contacts_json, "contacts_json")?;
+        let contacts = this.client.contacts()?;
+        put_json(out, &json::list(&contacts, json::contact));
+        Ok(())
+    })
+}
+
+// ----- sending -----
+
+/// Queues a text message; `*message_json` gets the local echo (its `clientMsgId` is the
+/// handle for edit, delete and cancel). Sent once connected, in order.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_send(
+    client: *const PushClient,
+    conversation_id_text: *const c_char,
+    message_text: *const c_char,
+    message_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(message_json, "message_json")?;
+        let id = conversation_id(conversation_id_text)?;
+        let body = unsafe { text(message_text, "text") }?;
+        let message = this.client.send(id, body)?;
+        put_json(out, &json::message(&message));
+        Ok(())
+    })
+}
+
+/// Changes the text of one of the user's messages; `*change_id` gets the id of the change.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_edit(
+    client: *const PushClient,
+    conversation_id_text: *const c_char,
+    target_client_msg_id: *const c_char,
+    message_text: *const c_char,
+    change_id: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(change_id, "change_id")?;
+        let id = conversation_id(conversation_id_text)?;
+        let target = client_msg_id(target_client_msg_id, "target_client_msg_id")?;
+        let body = unsafe { text(message_text, "text") }?;
+        let change = this.client.edit(id, target, body)?;
+        unsafe { *out = owned(change.to_string()) };
+        Ok(())
+    })
+}
+
+/// Deletes one of the user's messages; `*change_id` gets the id of the change.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_delete(
+    client: *const PushClient,
+    conversation_id_text: *const c_char,
+    target_client_msg_id: *const c_char,
+    change_id: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(change_id, "change_id")?;
+        let id = conversation_id(conversation_id_text)?;
+        let target = client_msg_id(target_client_msg_id, "target_client_msg_id")?;
+        let change = this.client.delete(id, target)?;
+        unsafe { *out = owned(change.to_string()) };
+        Ok(())
+    })
+}
+
+/// Takes back a message never sent; `*cancelled` says whether it was.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_cancel(
+    client: *const PushClient,
+    conversation_id_text: *const c_char,
+    message_client_msg_id: *const c_char,
+    cancelled: *mut bool,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(cancelled, "cancelled")?;
+        let id = conversation_id(conversation_id_text)?;
+        let message = client_msg_id(message_client_msg_id, "client_msg_id")?;
+        let taken_back = this.client.cancel(id, message)?;
+        unsafe { *out = taken_back };
+        Ok(())
+    })
+}
+
+/// Marks the conversation read up to `seq`.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_mark_read(
+    client: *const PushClient,
+    conversation_id_text: *const c_char,
+    seq: i64,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let id = conversation_id(conversation_id_text)?;
+        this.client.mark_read(id, seq)
+    })
+}
+
+// ----- requests (blocking) -----
+
+/// Lists the conversations from the server, stores what changed and returns all, as a
+/// JSON array.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_list_conversations(
+    client: *const PushClient,
+    conversations_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(conversations_json, "conversations_json")?;
+        let conversations = wait(this.client.list_conversations())?;
+        put_json(out, &json::list(&conversations, json::conversation));
+        Ok(())
+    })
+}
+
+/// Loads up to `limit` older messages before `*before_seq` (NULL: the newest) from the
+/// server and stores them; oldest first, as a JSON array.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_fetch_history(
+    client: *const PushClient,
+    conversation_id_text: *const c_char,
+    before_seq: *const i64,
+    limit: u32,
+    messages_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(messages_json, "messages_json")?;
+        let id = conversation_id(conversation_id_text)?;
+        let before = unsafe { before_seq.as_ref() }.copied();
+        let messages = wait(this.client.fetch_history(id, before, limit))?;
+        put_json(out, &json::list(&messages, json::message));
+        Ok(())
+    })
+}
+
+/// The direct conversation with an accepted contact, created the first time, as JSON.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_open_direct(
+    client: *const PushClient,
+    peer_user_id: *const c_char,
+    conversation_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(conversation_json, "conversation_json")?;
+        let peer = user_id(peer_user_id)?;
+        let conversation = wait(this.client.open_direct(peer))?;
+        put_json(out, &json::conversation(&conversation));
+        Ok(())
+    })
+}
+
+/// Lists the user's relationships from the server, as a JSON array.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_list_contacts(
+    client: *const PushClient,
+    contacts_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(contacts_json, "contacts_json")?;
+        let contacts = wait(this.client.list_contacts())?;
+        put_json(out, &json::list(&contacts, json::contact));
+        Ok(())
+    })
+}
+
+/// What a contact call does.
+#[derive(Clone, Copy)]
+enum ContactCall {
+    Request,
+    Accept,
+    Decline,
+    Block,
+    Unblock,
+}
+
+fn contact_call(
+    client: *const PushClient,
+    call: ContactCall,
+    user: *const c_char,
+    contact_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(contact_json, "contact_json")?;
+        let user = user_id(user)?;
+        let client = &this.client;
+        let contact = wait(async move {
+            match call {
+                ContactCall::Request => client.request_contact(user).await,
+                ContactCall::Accept => client.accept_contact(user).await,
+                ContactCall::Decline => client.decline_contact(user).await,
+                ContactCall::Block => client.block(user).await,
+                ContactCall::Unblock => client.unblock(user).await,
+            }
+        })?;
+        put_json(out, &json::contact(&contact));
+        Ok(())
+    })
+}
+
+/// Asks another user to become a contact; `*contact_json` gets the relationship now.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_request_contact(
+    client: *const PushClient,
+    user: *const c_char,
+    contact_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    contact_call(client, ContactCall::Request, user, contact_json, error)
+}
+
+/// Accepts a contact request.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_accept_contact(
+    client: *const PushClient,
+    user: *const c_char,
+    contact_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    contact_call(client, ContactCall::Accept, user, contact_json, error)
+}
+
+/// Declines a contact request.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_decline_contact(
+    client: *const PushClient,
+    user: *const c_char,
+    contact_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    contact_call(client, ContactCall::Decline, user, contact_json, error)
+}
+
+/// Blocks a user.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_block(
+    client: *const PushClient,
+    user: *const c_char,
+    contact_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    contact_call(client, ContactCall::Block, user, contact_json, error)
+}
+
+/// Unblocks a user.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_unblock(
+    client: *const PushClient,
+    user: *const c_char,
+    contact_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    contact_call(client, ContactCall::Unblock, user, contact_json, error)
+}
+
+/// Creates an invite code valid for `expires_in_s` seconds and `max_uses` uses, as JSON.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_create_invite(
+    client: *const PushClient,
+    expires_in_s: u32,
+    max_uses: u32,
+    invite_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(invite_json, "invite_json")?;
+        let invite = wait(this.client.create_invite(expires_in_s, max_uses))?;
+        put_json(out, &json::invite(&invite));
+        Ok(())
+    })
+}
+
+/// Redeems an invite code; `*inviter_user_id` gets its creator, now a contact.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_redeem_invite(
+    client: *const PushClient,
+    code: *const c_char,
+    inviter_user_id: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(inviter_user_id, "inviter_user_id")?;
+        let code = unsafe { text(code, "code") }?;
+        let inviter = wait(this.client.redeem_invite(code))?;
+        unsafe { *out = owned(inviter.to_string()) };
+        Ok(())
+    })
+}
+
+/// Revokes one of the user's invites.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_revoke_invite(
+    client: *const PushClient,
+    code: *const c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let code = unsafe { text(code, "code") }?;
+        wait(this.client.revoke_invite(code))
+    })
+}
+
+/// The profiles among `count` user ids the user may see, as a JSON array.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_profiles(
+    client: *const PushClient,
+    user_ids: *const *const c_char,
+    count: usize,
+    profiles_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(profiles_json, "profiles_json")?;
+        let ids = if count == 0 {
+            &[][..]
+        } else if user_ids.is_null() {
+            return Err(SdkError::Invalid("user_ids is NULL".to_owned()));
+        } else {
+            unsafe { std::slice::from_raw_parts(user_ids, count) }
+        };
+        let ids = ids
+            .iter()
+            .map(|id| user_id(*id))
+            .collect::<Result<Vec<_>, _>>()?;
+        let profiles = wait(this.client.profiles(ids))?;
+        put_json(out, &json::list(&profiles, json::profile));
+        Ok(())
+    })
+}
+
+/// The profile with a handle, as JSON.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_lookup(
+    client: *const PushClient,
+    handle_name: *const c_char,
+    profile_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(profile_json, "profile_json")?;
+        let handle_name = unsafe { text(handle_name, "handle_name") }?;
+        let profile = wait(this.client.lookup(handle_name))?;
+        put_json(out, &json::profile(&profile));
+        Ok(())
+    })
+}
+
+/// The account's devices, as a JSON array.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_devices(
+    client: *const PushClient,
+    devices_json: *mut *mut c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let out = place(devices_json, "devices_json")?;
+        let devices = wait(this.client.devices())?;
+        put_json(out, &json::list(&devices, json::device));
+        Ok(())
+    })
+}
+
+/// Revokes one of the account's devices.
+#[unsafe(no_mangle)]
+pub unsafe extern "C" fn push_client_revoke_device(
+    client: *const PushClient,
+    device_id: *const c_char,
+    error: *mut *mut PushError,
+) -> i32 {
+    guard(error, || {
+        let this = unsafe { handle(client) }?;
+        let device: DeviceId = json::id(unsafe { text(device_id, "device_id") }?, "device_id")?;
+        wait(this.client.revoke_device(device))
+    })
+}
```

Create `bindings/c/tests/c/flow.c`:

```c
/*
 * A C app on push-sdk's C API, run by tests/c_program.rs against the SDK's fake auth-svc and
 * gateway: usage `flow <authUrl> <socketUrl> <scratch directory>`. Prints one line per
 * step and "c flow: PASS" at the end; exits 1 at the first failure.
 */
#include "push_sdk.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
typedef CRITICAL_SECTION lock_t;
static void lock_init(lock_t *lock) { InitializeCriticalSection(lock); }
static void lock_take(lock_t *lock) { EnterCriticalSection(lock); }
static void lock_give(lock_t *lock) { LeaveCriticalSection(lock); }
static void pause_ms(unsigned ms) { Sleep(ms); }
#else
#include <pthread.h>
#include <time.h>
typedef pthread_mutex_t lock_t;
static void lock_init(lock_t *lock) { pthread_mutex_init(lock, NULL); }
static void lock_take(lock_t *lock) { pthread_mutex_lock(lock); }
static void lock_give(lock_t *lock) { pthread_mutex_unlock(lock); }
static void pause_ms(unsigned ms) {
    struct timespec wait = {(time_t)(ms / 1000), (long)(ms % 1000) * 1000000L};
    nanosleep(&wait, NULL);
}
#endif

static const char *auth_url;
static const char *socket_url;
static const char *scratch;

static void fail(const char *step, PushError *error) {
    fprintf(stderr, "c flow: FAIL at %s: %s (%s)\n", step,
            error ? push_error_message(error) : "unexpected result",
            error ? push_error_code(error) : "-");
    exit(1);
}

static void step(const char *done) { printf("c flow: %s\n", done); }

/* The value of a string field in a JSON object (enough for the SDK's flat ids). */
static int field(const char *json, const char *name, char *out, size_t size) {
    char pattern[64];
    const char *start;
    const char *end;
    snprintf(pattern, sizeof pattern, "\"%s\":\"", name);
    start = strstr(json, pattern);
    if (!start) return 0;
    start += strlen(pattern);
    end = strchr(start, '"');
    if (!end || (size_t)(end - start) >= size) return 0;
    memcpy(out, start, (size_t)(end - start));
    out[end - start] = '\0';
    return 1;
}

/*
 * Expects a failure with `code`. Takes the error's address, read once the call returned: C
 * leaves the order in which a call's arguments are evaluated open.
 */
static void expect_code(const char *what, int status, PushError **error, const char *code) {
    if (status != PUSH_ERROR || !*error || strcmp(push_error_code(*error), code) != 0) {
        fprintf(stderr, "c flow: FAIL at %s: wanted %s, got %s\n", what, code,
                *error ? push_error_code(*error) : "success");
        exit(1);
    }
    printf("c flow: %s failed with %s (retryable: %s)\n", what, code,
           push_error_retryable(*error) ? "yes" : "no");
    push_error_free(*error);
    *error = NULL;
}

/* ----- an in-memory secret store, as a keychain would be ----- */

struct entry {
    char key[64];
    uint8_t *value;
    size_t len;
};

struct store {
    lock_t lock;
    struct entry entries[8];
    int saves;
    int released;
};

static struct entry *find(struct store *store, const char *key) {
    int i;
    for (i = 0; i < 8; i++) {
        if (store->entries[i].value && strcmp(store->entries[i].key, key) == 0) return &store->entries[i];
    }
    return NULL;
}

static int32_t store_load(void *context, const char *key, uint8_t **value, size_t *value_len) {
    struct store *store = context;
    struct entry *found;
    int32_t status = PUSH_NOT_FOUND;
    lock_take(&store->lock);
    found = find(store, key);
    if (found) {
        *value = malloc(found->len ? found->len : 1);
        memcpy(*value, found->value, found->len);
        *value_len = found->len;
        status = PUSH_OK;
    }
    lock_give(&store->lock);
    return status;
}

static void store_free_value(void *context, uint8_t *value, size_t value_len) {
    (void)context;
    (void)value_len;
    free(value);
}

static int32_t store_remove(void *context, const char *key) {
    struct store *store = context;
    struct entry *found;
    lock_take(&store->lock);
    found = find(store, key);
    if (found) {
        free(found->value);
        found->value = NULL;
    }
    lock_give(&store->lock);
    return PUSH_OK;
}

static int32_t store_save(void *context, const char *key, const uint8_t *value, size_t value_len) {
    struct store *store = context;
    int i;
    store_remove(context, key);
    lock_take(&store->lock);
    for (i = 0; i < 8; i++) {
        if (!store->entries[i].value) {
            snprintf(store->entries[i].key, sizeof store->entries[i].key, "%s", key);
            store->entries[i].value = malloc(value_len ? value_len : 1);
            memcpy(store->entries[i].value, value, value_len);
            store->entries[i].len = value_len;
            store->saves++;
            break;
        }
    }
    lock_give(&store->lock);
    return i < 8 ? PUSH_OK : PUSH_ERROR;
}

static void store_release(void *context) {
    struct store *store = context;
    lock_take(&store->lock);
    store->released++;
    lock_give(&store->lock);
}

static PushSecretStore secret_store(struct store *store) {
    PushSecretStore functions;
    functions.context = store;
    functions.load = store_load;
    functions.save = store_save;
    functions.remove = store_remove;
    functions.free_value = store_free_value;
    functions.release = store_release;
    return functions;
}

static PushClient *open_client(struct store *store, const char *database, const uint8_t *key) {
    char config[1024];
    PushClient *client = NULL;
    PushError *error = NULL;
    snprintf(config, sizeof config,
             "{\"authUrl\":\"%s\",\"socketUrl\":\"%s\",\"databasePath\":\"%s\"}", auth_url,
             socket_url, database);
    if (push_client_open(config, key, key ? PUSH_DATABASE_KEY_LEN : 0, secret_store(store), &client,
                         &error) != PUSH_OK)
        fail("open", error);
    return client;
}

/* ----- events through a callback ----- */

struct seen {
    lock_t lock;
    int connected;
    int released;
};

static void on_event(void *context, const char *event_json) {
    struct seen *seen = context;
    lock_take(&seen->lock);
    if (strstr(event_json, "\"type\":\"connectionStateChanged\"") &&
        strstr(event_json, "\"state\":\"connected\""))
        seen->connected++;
    lock_give(&seen->lock);
}

static void on_release(void *context) {
    struct seen *seen = context;
    lock_take(&seen->lock);
    seen->released++;
    lock_give(&seen->lock);
}

int main(int argc, char **argv) {
    static struct store store;
    static struct store other_store;
    static struct store key_store;
    static struct seen seen;
    PushClient *client;
    PushError *error = NULL;
    char *text = NULL;
    char database[512];
    char conversation[64];
    char client_msg_id[64];
    char user_id[64];
    int sent = 0, answered = 0, waited;
    uint8_t salt[PUSH_SALT_LEN];
    uint8_t key[PUSH_DATABASE_KEY_LEN];
    uint8_t wrong_key[PUSH_DATABASE_KEY_LEN];

    if (argc != 4) {
        fprintf(stderr, "usage: flow <authUrl> <socketUrl> <scratch directory>\n");
        return 2;
    }
    auth_url = argv[1];
    socket_url = argv[2];
    scratch = argv[3];
    lock_init(&store.lock);
    lock_init(&other_store.lock);
    lock_init(&key_store.lock);
    lock_init(&seen.lock);
    printf("c flow: push-sdk %s\n", push_version());

    /* Refusals carry the SDK's codes. */
    expect_code("an open with a config that is not JSON",
                push_client_open("{", NULL, 0, secret_store(&store), &client, &error), &error,
                "invalid");
    if (store.released != 1) fail("the store is released when the open fails", NULL);
    store.released = 0;

    client = open_client(&store, ":memory:", NULL);
    error = NULL;
    expect_code("a sign-in with a wrong password",
                push_client_sign_in(client, "c@example.com", "wrong password", "c app", "desktop",
                                    &text, &error),
                &error, "invalid_credentials");
    error = NULL;
    if (push_client_sign_in(client, "c@example.com", "password", "c app", "desktop", &text,
                            &error) != PUSH_OK)
        fail("sign in", error);
    if (!field(text, "userId", user_id, sizeof user_id)) fail("the account names the user", NULL);
    push_string_free(text);
    if (store.saves < 1) fail("the session is saved in the app's store", NULL);
    step("signed in; the session is in the app's secret store");

    if (push_client_connect(client, &error) != PUSH_OK) fail("connect", error);
    if (push_client_wait_connected(client, 10000, &error) != PUSH_OK) fail("wait connected", error);
    if (push_client_open_direct(client, "0199e1c2-0000-7000-8000-000000000001", &text, &error) !=
        PUSH_OK)
        fail("open a direct conversation", error);
    if (!field(text, "id", conversation, sizeof conversation)) fail("the conversation's id", NULL);
    push_string_free(text);
    step("connected and opened a direct conversation");

    if (push_client_send(client, conversation, "hello from C", &text, &error) != PUSH_OK)
        fail("send", error);
    if (!field(text, "clientMsgId", client_msg_id, sizeof client_msg_id) ||
        !strstr(text, "\"status\":\"pending\""))
        fail("the local echo", NULL);
    push_string_free(text);
    for (waited = 0; waited < 100 && !(sent && answered); waited++) {
        char *event = NULL;
        if (push_client_next_event(client, 100, &event, &error) != PUSH_OK) fail("next event", error);
        if (!event) continue;
        if (strstr(event, "\"type\":\"messageSent\"") && strstr(event, client_msg_id)) sent = 1;
        if (strstr(event, "\"type\":\"messageAdded\"") && strstr(event, "hello from C") &&
            !strstr(event, user_id))
            answered = 1;
        push_string_free(event);
    }
    if (!sent || !answered) fail("messageSent and the peer's answer as events", NULL);
    if (push_client_messages(client, conversation, NULL, 10, &text, &error) != PUSH_OK)
        fail("messages", error);
    if (!strstr(text, "\"seq\":1") || !strstr(text, "\"seq\":2")) fail("both messages stored", NULL);
    push_string_free(text);
    step("sent a message: messageSent (seq 1), the peer's answer (seq 2), both stored");

    expect_code("a send to an unknown conversation",
                push_client_send(client, "0199e1c2-0000-7000-8000-0000000000ff", "x", &text, &error),
                &error, "invalid");
    error = NULL;
    expect_code("redeeming an unknown invite",
                push_client_redeem_invite(client, "NOSUCHCODE", &text, &error), &error,
                "invite_not_found");
    error = NULL;
    expect_code("a callback after polling",
                push_client_set_event_callback(client, on_event, &seen, on_release, &error), &error,
                "invalid");
    error = NULL;
    push_client_destroy(client);
    if (store.released != 1) fail("the store is released by destroy", NULL);
    step("destroyed the client; the store was released");

    /* Events through a callback, on an SDK thread. */
    client = open_client(&other_store, ":memory:", NULL);
    if (push_client_set_event_callback(client, on_event, &seen, on_release, &error) != PUSH_OK)
        fail("set the event callback", error);
    if (push_client_sign_in(client, "c@example.com", "password", "c app", "desktop", &text,
                            &error) != PUSH_OK)
        fail("sign in again", error);
    push_string_free(text);
    if (push_client_connect(client, &error) != PUSH_OK) fail("connect again", error);
    for (waited = 0; waited < 100; waited++) {
        int connected;
        lock_take(&seen.lock);
        connected = seen.connected;
        lock_give(&seen.lock);
        if (connected) break;
        pause_ms(100);
    }
    if (!seen.connected) fail("the callback heard connected", NULL);
    push_client_destroy(client);
    for (waited = 0; waited < 100 && !seen.released; waited++) pause_ms(50);
    if (seen.released != 1) fail("the callback's context is released after the last event", NULL);
    step("the callback heard connected on an SDK thread, then its context was released");

    /* An encrypted database opens only with its key. */
    if (push_new_salt(salt) != PUSH_OK) fail("new salt", NULL);
    if (push_derive_database_key((const uint8_t *)"2468", 4, salt, sizeof salt, key, &error) !=
        PUSH_OK)
        fail("derive a key", error);
    if (push_derive_database_key((const uint8_t *)"1357", 4, salt, sizeof salt, wrong_key,
                                 &error) != PUSH_OK)
        fail("derive another key", error);
    snprintf(database, sizeof database, "%s/encrypted.db", scratch);
    client = open_client(&key_store, database, key);
    push_client_destroy(client);
    {
        char config[1024];
        snprintf(config, sizeof config,
                 "{\"authUrl\":\"%s\",\"socketUrl\":\"%s\",\"databasePath\":\"%s\"}", auth_url,
                 socket_url, database);
        expect_code("an open with the wrong key",
                    push_client_open(config, wrong_key, sizeof wrong_key, secret_store(&key_store),
                                     &client, &error),
                    &error, "wrong_key");
    }
    client = open_client(&key_store, database, key);
    push_client_destroy(client);
    step("an encrypted database opened with its key, and only with it");

    printf("c flow: PASS\n");
    return 0;
}
```

Create `bindings/c/tests/c_program.rs`:

```rust
//! Compiles `tests/c/flow.c` against `include/push_sdk.h`, links it to the shared library
//! cargo built for these tests, and runs it against the SDK's fake auth-svc and gateway:
//! a C app signs in with its own secret store, connects, sends, reads events by polling and
//! through a callback, and opens an encrypted database, checking the error codes on the
//! way.

use std::path::{Path, PathBuf};
use std::process::Command;

use push_sdk::testing::{AutoGateway, FakeAuth};

/// target/<profile>, where cargo puts the shared library.
fn library_dir() -> PathBuf {
    let test = std::env::current_exe().unwrap();
    test.parent().and_then(Path::parent).unwrap().to_path_buf()
}

/// Builds the shared library: cargo builds only the Rust library for integration tests.
fn build_library() {
    let library = library_dir();
    let mut command = Command::new(env!("CARGO"));
    command
        .args([
            "build",
            "--locked",
            "--lib",
            "-p",
            "push-sdk-c",
            "--target-dir",
        ])
        .arg(library.parent().unwrap());
    if library.ends_with("release") {
        command.arg("--release");
    }
    let built = command.output().expect("cargo runs");
    assert!(
        built.status.success(),
        "the shared library does not build:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
}

fn compile(scratch: &Path) -> PathBuf {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = env!("PUSH_SDK_C_TARGET");
    let compiler = cc::Build::new()
        .target(target)
        .host(target)
        .opt_level(0)
        .cargo_metadata(false)
        .cargo_warnings(false)
        .get_compiler();
    let source = crate_dir.join("tests").join("c").join("flow.c");
    let include = crate_dir.join("include");
    let library = library_dir();
    let program = scratch.join(if cfg!(windows) { "flow.exe" } else { "flow" });
    let mut command = compiler.to_command();
    if compiler.is_like_msvc() {
        command
            .arg("/nologo")
            .arg(format!("/I{}", include.display()))
            .arg(&source)
            .arg(format!("/Fo{}\\", scratch.display()))
            .arg(format!("/Fe{}", program.display()))
            .arg("/link")
            .arg(library.join("push_sdk_c.dll.lib"));
    } else {
        command
            .arg("-std=c99")
            .arg("-D_POSIX_C_SOURCE=200809L")
            .arg("-Wall")
            .arg(format!("-I{}", include.display()))
            .arg(&source)
            .arg("-o")
            .arg(&program)
            .arg(format!("-L{}", library.display()))
            .arg("-lpush_sdk_c")
            .arg(format!("-Wl,-rpath,{}", library.display()))
            .arg("-lpthread");
    }
    let compiled = command.output().expect("the C compiler runs");
    assert!(
        compiled.status.success(),
        "the C program does not compile:\n{}{}",
        String::from_utf8_lossy(&compiled.stdout),
        String::from_utf8_lossy(&compiled.stderr)
    );
    program
}

#[test]
fn a_c_program_signs_in_sends_and_reads_events() {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("push-sdk-c-flow");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    build_library();
    let program = compile(&scratch);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (auth, gateway) = runtime.block_on(async {
        let auth = FakeAuth::start().await;
        let gateway = AutoGateway::start(auth.identity.clone()).await;
        (auth, gateway)
    });
    // The program finds the shared library next to it (Windows) or by its rpath.
    let mut path = std::ffi::OsString::from(library_dir());
    path.push(if cfg!(windows) { ";" } else { ":" });
    path.push(std::env::var_os("PATH").unwrap_or_default());
    // JSON in the program's config: forward slashes need no escaping.
    let scratch_arg = scratch.display().to_string().replace('\\', "/");
    let ran = Command::new(&program)
        .args([
            auth.url.as_str(),
            gateway.url.as_str(),
            scratch_arg.as_str(),
        ])
        .env("PATH", path)
        .output()
        .expect("the C program runs");
    let stdout = String::from_utf8_lossy(&ran.stdout);
    println!("{stdout}");
    assert!(
        ran.status.success() && stdout.contains("c flow: PASS"),
        "the C program failed:\n{stdout}{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    drop(runtime);
    let _ = std::fs::remove_dir_all(&scratch);
}
```

Regenerate the header:

```powershell
$env:PUSH_SDK_BLESS_HEADER = '1'
cargo test -p push-sdk-c --test header --locked
Remove-Item Env:PUSH_SDK_BLESS_HEADER
(Get-FileHash bindings/c/include/push_sdk.h -Algorithm SHA256).Hash
```

Expected: the hash is `1FA8A1AE0F8009AEE728131A4F167CE66BD83DA35E672ED6ECD0ACDA0E23D661` (374 lines).

Run the C program:

```powershell
cargo test -p push-sdk-c --test c_program --locked -- --nocapture
```

Expected (ids differ):

```
c flow: push-sdk 0.1.0
c flow: an open with a config that is not JSON failed with invalid (retryable: no)
c flow: a sign-in with a wrong password failed with invalid_credentials (retryable: no)
c flow: signed in; the session is in the app's secret store
c flow: connected and opened a direct conversation
c flow: sent a message: messageSent (seq 1), the peer's answer (seq 2), both stored
c flow: a send to an unknown conversation failed with invalid (retryable: no)
c flow: redeeming an unknown invite failed with invite_not_found (retryable: no)
c flow: a callback after polling failed with invalid (retryable: no)
c flow: destroyed the client; the store was released
c flow: the callback heard connected on an SDK thread, then its context was released
c flow: an open with the wrong key failed with wrong_key (retryable: no)
c flow: an encrypted database opened with its key, and only with it
c flow: PASS
```

- [ ] **Step 2: Red — a send that loses its text**

In `bindings/c/src/client.rs`, `push_client_send`, replace `this.client.send(id, body)?`
with `this.client.send(id, "")?` and run the command above. Expected: FAILED,
`c flow: FAIL at send: invalid: message body is empty (invalid)`. Restore the line.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A Cargo.lock bindings
git commit -m "push-sdk-c: the local data, sending and the requests; a C program run against the fakes"
```

Expected: 977 tests pass, no warnings, no diff.

---

### Task 5: The Node binding: promises, events, the secret store bridge

**Files:**
- Create: `bindings/node/` — `.gitignore`, `Cargo.toml`, `build.rs`, `package.json`,
  `index.js`, `scripts/build.js`, `src/lib.rs`, `src/client.rs`,
  `src/promise.rs`, `src/secrets.rs`, `test/support.js`, `test/client.test.js`
- Modify: `Cargo.toml`, `Cargo.lock`

The addon, the bridge (decision 3), events (decision 4), errors (decision 5) and the
account and connection calls; `index.js` is what apps import. The typings come with
Task 6, which checks them.

- [ ] **Step 1: Write the code**

Change `Cargo.lock` (save the block to a file and `git apply` it):

```diff
diff --git a/Cargo.lock b/Cargo.lock
index 7a07f20..62af290 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -525,6 +525,15 @@ version = "0.9.6"
 source = "registry+https://github.com/rust-lang/crates.io-index"
 checksum = "c2459377285ad874054d797f3ccebf984978aa39129f6eafde5cdc8315b612f8"
 
+[[package]]
+name = "convert_case"
+version = "0.8.0"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "baaaa0ecca5b51987b9423ccdc971514dd8b0bb7b4060b983d3664dad3f1f89f"
+dependencies = [
+ "unicode-segmentation",
+]
+
 [[package]]
 name = "core-foundation"
 version = "0.10.1"
@@ -599,6 +608,22 @@ dependencies = [
  "typenum",
 ]
 
+[[package]]
+name = "ctor"
+version = "0.6.3"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "424e0138278faeb2b401f174ad17e715c829512d74f3d1e81eb43365c2e0590e"
+dependencies = [
+ "ctor-proc-macro",
+ "dtor",
+]
+
+[[package]]
+name = "ctor-proc-macro"
+version = "0.0.7"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "52560adf09603e58c9a7ee1fe1dcb95a16927b17c127f0ac02d6e768a0e25bc1"
+
 [[package]]
 name = "darling"
 version = "0.21.3"
@@ -701,6 +726,21 @@ version = "0.15.7"
 source = "registry+https://github.com/rust-lang/crates.io-index"
 checksum = "1aaf95b3e5c8f23aa320147307562d361db0ae0d51242340f558153b4eb2439b"
 
+[[package]]
+name = "dtor"
+version = "0.1.1"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "404d02eeb088a82cfd873006cb713fe411306c7d182c344905e101fb1167d301"
+dependencies = [
+ "dtor-proc-macro",
+]
+
+[[package]]
+name = "dtor-proc-macro"
+version = "0.0.6"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "f678cf4a922c215c63e0de95eb1ff08a958a81d47e485cf9da1e27bf6305cfa5"
+
 [[package]]
 name = "dyn-clone"
 version = "1.0.20"
@@ -1478,6 +1518,16 @@ version = "0.2.190"
 source = "registry+https://github.com/rust-lang/crates.io-index"
 checksum = "ce5d3ddc6d3fa000eb1536d85e147bfe31aacaba692ed6a876f95cb7c855be78"
 
+[[package]]
+name = "libloading"
+version = "0.8.9"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "d7c4b02199fee7c5d21a5ae7d8cfa79a6ef5bb2fc834d6e9058e89c825efdc55"
+dependencies = [
+ "cfg-if",
+ "windows-link",
+]
+
 [[package]]
 name = "libm"
 version = "0.2.16"
@@ -1605,6 +1655,70 @@ version = "0.10.1"
 source = "registry+https://github.com/rust-lang/crates.io-index"
 checksum = "1d87ecb2933e8aeadb3e3a02b828fed80a7528047e68b4f424523a0981a3a084"
 
+[[package]]
+name = "napi"
+version = "3.4.0"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "c3a1135cfe16ca43ac82ac05858554fc39c037d8e4592f2b4a83d7ef8e822f43"
+dependencies = [
+ "bitflags 2.13.2",
+ "ctor",
+ "napi-build",
+ "napi-sys",
+ "nohash-hasher",
+ "rustc-hash",
+ "serde",
+ "serde_json",
+]
+
+[[package]]
+name = "napi-build"
+version = "2.2.4"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "3ae82775d1b06f3f07efd0666e59bbc175da8383bc372051031d7a447e94fbea"
+
+[[package]]
+name = "napi-derive"
+version = "3.3.0"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "78665d6bdf10e9a4e6b38123efb0f66962e6197c1aea2f07cff3f159a374696d"
+dependencies = [
+ "convert_case",
+ "ctor",
+ "napi-derive-backend",
+ "proc-macro2",
+ "quote",
+ "syn 2.0.119",
+]
+
+[[package]]
+name = "napi-derive-backend"
+version = "3.0.0"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "42d55d01423e7264de3acc13b258fa48ca7cf38a4d25db848908ec3c1304a85a"
+dependencies = [
+ "convert_case",
+ "proc-macro2",
+ "quote",
+ "semver",
+ "syn 2.0.119",
+]
+
+[[package]]
+name = "napi-sys"
+version = "3.0.1"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "1ed8f0e23a62a3ce0fbb6527cdc056e9282ddd9916b068c46f8923e18eed5ee6"
+dependencies = [
+ "libloading",
+]
+
+[[package]]
+name = "nohash-hasher"
+version = "0.2.0"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "2bf50223579dc7cdcfb3bfcacf7069ff68243f8c363f62ffa99cf000a6b9c451"
+
 [[package]]
 name = "nu-ansi-term"
 version = "0.50.3"
@@ -2268,6 +2382,19 @@ dependencies = [
  "tokio",
 ]
 
+[[package]]
+name = "push-sdk-node"
+version = "0.1.0"
+dependencies = [
+ "napi",
+ "napi-build",
+ "napi-derive",
+ "push-sdk",
+ "push-sdk-ffi",
+ "serde_json",
+ "tokio",
+]
+
 [[package]]
 name = "push-smoke"
 version = "0.1.0"
@@ -2850,6 +2977,12 @@ dependencies = [
  "libc",
 ]
 
+[[package]]
+name = "semver"
+version = "1.0.28"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "8a7852d02fc848982e0c167ef163aaff9cd91dc640ba85e263cb1ce46fae51cd"
+
 [[package]]
 name = "serde"
 version = "1.0.229"
@@ -3923,6 +4056,12 @@ version = "0.1.4"
 source = "registry+https://github.com/rust-lang/crates.io-index"
 checksum = "7df058c713841ad818f1dc5d3fd88063241cc61f49f5fbea4b951e8cf5a8d71d"
 
+[[package]]
+name = "unicode-segmentation"
+version = "1.13.3"
+source = "registry+https://github.com/rust-lang/crates.io-index"
+checksum = "c6f5d3c3b1bf09027a88a6bc961fc00497d651009560b5463668dc81b0fa87a8"
+
 [[package]]
 name = "untrusted"
 version = "0.9.0"
```

Change `Cargo.toml` (save the block to a file and `git apply` it):

```diff
diff --git a/Cargo.toml b/Cargo.toml
index 040d3d0..be765e9 100644
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -3,6 +3,7 @@ resolver = "3"
 members = [
     "bindings/c",
     "bindings/ffi",
+    "bindings/node",
     "crates/push-auth",
     "crates/push-domain",
     "crates/push-email",
```

Create `bindings/node/.gitignore`:

```text
/node_modules/
/push_sdk.node
```

Create `bindings/node/Cargo.toml`:

```toml
[package]
name = "push-sdk-node"
description = "The Node.js binding of push-sdk (napi-rs): promises, events through a threadsafe function, the secret store as JavaScript callbacks (spec §8.5)."
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
publish = false

[lib]
# The addon Node loads (push_sdk.node, copied by scripts/build.js). No Rust tests: its
# test binary could not resolve Node's symbols; the tests run in Node (npm test).
crate-type = ["cdylib"]
test = false
doctest = false

[dependencies]
# napi 3.4, napi-derive 3.3 and napi-build 2.2 are the last releases for Rust 1.85 (later
# ones need 1.88); the lockfile pins them.
napi = { version = "3.4", default-features = false, features = ["napi4", "serde-json"] }
napi-derive = "3.3"
push-sdk.workspace = true
push-sdk-ffi.workspace = true
serde_json.workspace = true
# block_in_place, while an SDK thread waits for the app's secret store.
tokio = { workspace = true, features = ["rt-multi-thread"] }

[build-dependencies]
napi-build = "2.2"

[lints.clippy]
all = { level = "warn", priority = -1 }
```

Create `bindings/node/build.rs`:

```rust
fn main() {
    napi_build::setup();
}
```

Create `bindings/node/index.js`:

```js
'use strict';

/**
 * push-sdk for Node.js: the client core of push-messenger (spec §8), over the Rust SDK.
 *
 * - Every call that talks to a server or reads or writes the database returns a promise; the
 *   JavaScript thread never waits for the SDK.
 * - Failures are `PushError`s with the SDK's stable `code` (the same as in Rust and C),
 *   `retryable` and, when rate limited, `retryAfterS`.
 * - Events come, in order, to the one listener given to `onEvent`.
 * - Credentials live only in the app's secret store: an object with `load`, `save` and
 *   `delete` (each may return a promise). docs/sdk-secure-storage.md shows how to back it with
 *   the platform's keychain. The SDK loads the values once when the client opens and writes
 *   through to the store, so the store must not change behind its back while it is open.
 * - Close the client when done: while it has a listener, Node keeps running.
 *
 * docs/sdk-bindings.md describes every call and the shapes of the data.
 */

const path = require('node:path');

const native = require(path.join(__dirname, 'push_sdk.node'));

/** A failed call. `code` is the SDK's stable error code. */
class PushError extends Error {
  constructor(detail) {
    super(detail.message);
    this.name = 'PushError';
    this.code = detail.code;
    this.retryable = Boolean(detail.retryable);
    this.retryAfterS = detail.retryAfterS == null ? null : detail.retryAfterS;
  }
}

/** The native layer's error as a PushError; an argument it refused is `invalid`. */
function toPushError(error) {
  let detail = null;
  try {
    detail = JSON.parse(error.message);
  } catch {
    detail = null;
  }
  if (detail && typeof detail.code === 'string') {
    return new PushError(detail);
  }
  return new PushError({ code: 'invalid', message: String(error.message), retryable: false });
}

async function settled(call) {
  try {
    return await call();
  } catch (error) {
    throw toPushError(error);
  }
}

function now(call) {
  try {
    return call();
  } catch (error) {
    throw toPushError(error);
  }
}

/**
 * The function the native layer calls for the app's store: it runs the store's method and
 * answers with `completeSecretCall`, exactly once, whether the method returns, resolves,
 * throws or rejects.
 */
function secretBridge(store) {
  const operations = {
    load: (key) => store.load(key),
    save: (key, value) => store.save(key, value),
    delete: (key) => store.delete(key),
  };
  return (id, operation, key, value) => {
    Promise.resolve()
      .then(() => operations[operation](key, value))
      .then(
        (result) => {
          const loaded = operation === 'load' && result != null ? Buffer.from(result) : null;
          native.completeSecretCall(id, null, loaded);
        },
        (error) => native.completeSecretCall(id, String((error && error.message) || error), null),
      );
  };
}

function checkStore(store) {
  for (const name of ['load', 'save', 'delete']) {
    if (!store || typeof store[name] !== 'function') {
      throw new PushError({
        code: 'invalid',
        message: `the secret store has no ${name} function`,
        retryable: false,
      });
    }
  }
}

class Client {
  #native;

  /** Use `Client.open`. */
  constructor(nativeClient) {
    this.#native = nativeClient;
  }

  /**
   * Opens a client: loads the credentials from `secretStore`, then opens the database (off
   * the JavaScript thread: a database key is checked, the schema migrated). It does not
   * connect until `connect()`.
   */
  static async open(config, secretStore) {
    checkStore(secretStore);
    const { databaseKey, ...settings } = config || {};
    const key = databaseKey == null ? null : Buffer.from(databaseKey);
    const opened = await settled(() => native.open(settings, key, secretBridge(secretStore)));
    return new Client(opened);
  }

  /** Sends every event to `listener`, in order. Once per client. */
  onEvent(listener) {
    if (typeof listener !== 'function') {
      throw new PushError({ code: 'invalid', message: 'the listener is not a function', retryable: false });
    }
    now(() =>
      this.#native.onEvent((event) => {
        try {
          listener(event);
        } catch (error) {
          // A listener's own failure is the app's: reported like any uncaught exception.
          queueMicrotask(() => {
            throw error;
          });
        }
      }),
    );
  }

  /** Stops the connection and closes the database; later calls fail with `closed`. */
  close() {
    return settled(() => this.#native.close());
  }

  // ----- account -----

  signUp(email, password, handle, displayName) {
    return settled(() => this.#native.signUp(email, password, handle, displayName));
  }

  resendVerification(email) {
    return settled(() => this.#native.resendVerification(email));
  }

  forgotPassword(email) {
    return settled(() => this.#native.forgotPassword(email));
  }

  signIn(email, password, deviceName, platform) {
    return settled(() => this.#native.signIn(email, password, deviceName, platform));
  }

  /** The signed-in account, or null. Answered at once. */
  account() {
    return now(() => this.#native.account());
  }

  signOut() {
    return settled(() => this.#native.signOut());
  }

  // ----- connection -----

  /** Starts connecting and keeps reconnecting until `disconnect()`. Returns at once. */
  connect() {
    now(() => this.#native.connect());
  }

  disconnect() {
    now(() => this.#native.disconnect());
  }

  /** The connection's state now. */
  connectionState() {
    return now(() => this.#native.connectionState());
  }

  waitConnected(timeoutMs) {
    return settled(() => this.#native.waitConnected(timeoutMs));
  }

  changeDatabaseKey(databaseKey) {
    const key = databaseKey == null ? null : Buffer.from(databaseKey);
    return settled(() => this.#native.changeDatabaseKey(key));
  }

}

/** Derives a 32-byte database key from what the user knows and a salt the app keeps. */
function deriveDatabaseKey(secret, salt) {
  const asBytes = (value) => (typeof value === 'string' ? Buffer.from(value, 'utf8') : Buffer.from(value));
  return settled(() => native.deriveDatabaseKey(asBytes(secret), asBytes(salt)));
}

/** A new random salt for `deriveDatabaseKey`, to keep next to the database. */
function newSalt() {
  return native.newSalt();
}

/** The keys the SDK keeps in the secret store. */
const SECRET_KEYS = Object.freeze(native.secretKeys());

module.exports = { Client, PushError, deriveDatabaseKey, newSalt, SECRET_KEYS };
```

Create `bindings/node/package.json`:

```json
{
  "name": "push-sdk",
  "version": "0.1.0",
  "description": "push-messenger's client SDK for Node.js: a binding of the Rust core (push-sdk).",
  "private": true,
  "license": "MIT",
  "main": "index.js",
  "files": [
    "index.js",
    "push_sdk.node"
  ],
  "engines": {
    "node": ">=18"
  },
  "scripts": {
    "build": "node scripts/build.js",
    "pretest": "node scripts/build.js --with-fakes",
    "test": "node --test test/client.test.js"
  }
}
```

Create `bindings/node/scripts/build.js`:

```js
'use strict';

// Builds the addon with cargo and copies it next to index.js as push_sdk.node.
// `--release` builds optimised; `--with-fakes` also builds push-sdk-fakes, the fake auth-svc
// and gateway the tests run against.

const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const root = path.join(__dirname, '..', '..', '..');
const release = process.argv.includes('--release');
const profile = release ? 'release' : 'debug';
const cargo = (args) =>
  execFileSync('cargo', [...args, ...(release ? ['--release'] : [])], { cwd: root, stdio: 'inherit' });

cargo(['build', '--locked', '-p', 'push-sdk-node']);
if (process.argv.includes('--with-fakes')) {
  cargo(['build', '--locked', '-p', 'push-sdk', '--features', 'testing', '--bin', 'push-sdk-fakes']);
}

const targetDir = process.env.CARGO_TARGET_DIR || path.join(root, 'target');
const library = {
  win32: 'push_sdk_node.dll',
  darwin: 'libpush_sdk_node.dylib',
}[process.platform] || 'libpush_sdk_node.so';
fs.copyFileSync(path.join(targetDir, profile, library), path.join(__dirname, '..', 'push_sdk.node'));
```

Create `bindings/node/src/client.rs`:

```rust
//! The client, as a class JavaScript holds: `index.js` wraps it.

use std::future::Future;
use std::sync::Mutex;

use napi::bindgen_prelude::{Buffer, Function, Object};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Env, Status};
use napi_derive::napi;
use push_sdk::{Client, DatabaseKey, Events, SdkError};
use push_sdk_ffi::{block_on, json};
use serde_json::Value;

use crate::promise::{Settle, failure};

/// How many events may wait for the JavaScript thread; past that the events thread waits,
/// and the SDK's own queue (10,000, then `eventsLost`) takes over.
const EVENTS_IN_FLIGHT: usize = 1000;

type Listener = ThreadsafeFunction<Value, (), Value, Status, false, false, EVENTS_IN_FLIGHT>;

#[napi]
pub struct NativeClient {
    client: Mutex<Option<Client>>,
    events: Mutex<Option<Events>>,
}

fn invalid(message: &str) -> napi::Error {
    failure(&SdkError::Invalid(message.to_owned()))
}

impl NativeClient {
    pub fn new((client, events): (Client, Events)) -> Self {
        Self {
            client: Mutex::new(Some(client)),
            events: Mutex::new(Some(events)),
        }
    }

    fn client(&self) -> napi::Result<Client> {
        self.client
            .lock()
            .ok()
            .and_then(|client| client.clone())
            .ok_or_else(|| failure(&SdkError::Closed))
    }

    /// Runs `work` on the client's runtime; its result settles the promise.
    fn promise<'env, F>(
        &self,
        env: &'env Env,
        work: impl FnOnce(Client) -> F,
    ) -> napi::Result<Object<'env>>
    where
        F: Future<Output = Result<Value, SdkError>> + Send + 'static,
    {
        let client = self.client()?;
        let (settle, promise) = Settle::new(env)?;
        let future = work(client.clone());
        // On failure the task, and with it `settle`, is dropped: it rejects with `closed`.
        let _ = client.spawn(async move { settle.finish(future.await) });
        Ok(promise)
    }

    /// A call answered at once.
    fn now<T>(&self, work: impl FnOnce(&Client) -> Result<T, SdkError>) -> napi::Result<T> {
        work(&self.client()?).map_err(|error| failure(&error))
    }
}

#[napi]
impl NativeClient {
    /// Delivers every event, as JSON, to `listener` on the JavaScript thread, in order.
    /// Only one listener per client. While it listens, Node keeps running until `close`.
    #[napi(catch_unwind)]
    pub fn on_event(&self, listener: Function<'_, Value, ()>) -> napi::Result<()> {
        let mut events = self
            .events
            .lock()
            .ok()
            .and_then(|mut events| events.take())
            .ok_or_else(|| invalid("the events have a listener already"))?;
        let listener: Listener = listener
            .build_threadsafe_function::<Value>()
            .callee_handled::<false>()
            .max_queue_size::<EVENTS_IN_FLIGHT>()
            .build_callback(|context| Ok(context.value))?;
        std::thread::Builder::new()
            .name("push-sdk-events".to_owned())
            .spawn(move || {
                // Ends when the client is gone; dropping the listener lets Node exit.
                while let Some(event) = block_on(events.next()) {
                    let status =
                        listener.call(json::event(&event), ThreadsafeFunctionCallMode::Blocking);
                    if status != Status::Ok {
                        break;
                    }
                }
            })
            .map_err(|_| invalid("cannot start the events thread"))?;
        Ok(())
    }

    /// Stops the connection, closes the database and ends the client's threads; later calls
    /// fail with `closed`.
    #[napi(catch_unwind)]
    pub fn close<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
        let client = self.client.lock().ok().and_then(|mut client| client.take());
        if let Ok(mut events) = self.events.lock() {
            events.take();
        }
        let (settle, promise) = Settle::new(env)?;
        match client {
            None => settle.finish(Ok(Value::Null)),
            Some(client) => {
                std::thread::Builder::new()
                    .name("push-sdk-close".to_owned())
                    .spawn(move || settle.finish(block_on(client.close()).map(|()| Value::Null)))
                    .map_err(|_| invalid("cannot start a thread"))?;
            }
        }
        Ok(promise)
    }

    // ----- account -----

    #[napi(catch_unwind)]
    pub fn sign_up<'env>(
        &self,
        env: &'env Env,
        email: String,
        password: String,
        handle: String,
        display_name: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            client
                .sign_up(&email, &password, &handle, &display_name)
                .await
                .map(|()| Value::Null)
        })
    }

    #[napi(catch_unwind)]
    pub fn resend_verification<'env>(
        &self,
        env: &'env Env,
        email: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            client
                .resend_verification(&email)
                .await
                .map(|()| Value::Null)
        })
    }

    #[napi(catch_unwind)]
    pub fn forgot_password<'env>(
        &self,
        env: &'env Env,
        email: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            client.forgot_password(&email).await.map(|()| Value::Null)
        })
    }

    #[napi(catch_unwind)]
    pub fn sign_in<'env>(
        &self,
        env: &'env Env,
        email: String,
        password: String,
        device_name: String,
        platform: String,
    ) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            let platform = json::platform(&platform)?;
            let account = client
                .sign_in(&email, &password, &device_name, platform)
                .await?;
            Ok(json::account(&account))
        })
    }

    /// The signed-in account, or null: from the bridge's cache, so it never waits.
    #[napi(catch_unwind)]
    pub fn account(&self) -> napi::Result<Value> {
        self.now(|client| {
            Ok(client
                .account()?
                .as_ref()
                .map_or(Value::Null, json::account))
        })
    }

    #[napi(catch_unwind)]
    pub fn sign_out<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
        self.promise(env, |client| async move {
            client.sign_out().await.map(|()| Value::Null)
        })
    }

    // ----- connection -----

    #[napi(catch_unwind)]
    pub fn connect(&self) -> napi::Result<()> {
        self.now(Client::connect)
    }

    #[napi(catch_unwind)]
    pub fn disconnect(&self) -> napi::Result<()> {
        self.now(Client::disconnect)
    }

    #[napi(catch_unwind)]
    pub fn connection_state(&self) -> napi::Result<Value> {
        self.now(|client| Ok(json::connection_state(client.connection_state())))
    }

    #[napi(catch_unwind)]
    pub fn wait_connected<'env>(
        &self,
        env: &'env Env,
        timeout_ms: u32,
    ) -> napi::Result<Object<'env>> {
        let timeout = std::time::Duration::from_millis(u64::from(timeout_ms));
        self.promise(env, |client| async move {
            client.wait_connected(timeout).await.map(|()| Value::Null)
        })
    }

    /// Encrypts the database with a new key of 32 bytes, or without one decrypts it.
    #[napi(catch_unwind)]
    pub fn change_database_key<'env>(
        &self,
        env: &'env Env,
        database_key: Option<Buffer>,
    ) -> napi::Result<Object<'env>> {
        let key = database_key
            .as_deref()
            .map(DatabaseKey::from_bytes)
            .transpose()
            .map_err(|error| failure(&error))?;
        self.promise(env, |client| async move {
            client
                .change_database_key(key.as_ref())
                .await
                .map(|()| Value::Null)
        })
    }
}
```

Create `bindings/node/src/lib.rs`:

```rust
//! push-sdk's Node.js binding (spec §8.5), wrapped by `index.js`, which apps import.
//!
//! - **Promises.** Every call that talks to a server or touches the database returns a
//!   promise settled from the client's own runtime ([`push_sdk::Client::spawn`]): the
//!   JavaScript thread never waits for the SDK, and the binding has no runtime of its own.
//! - **Errors** reject (or throw) with an `Error` whose message is the JSON of
//!   [`push_sdk_ffi::json::error`]; `index.js` turns it into a `PushError` with the SDK's
//!   stable `code`.
//! - **Events** go to one listener through a threadsafe function ([`client`]).
//! - **The secret store** is the app's JavaScript object, bridged by [`secrets`].
//! - **Opening** runs on a thread of its own: it may stretch a database key and migrate the
//!   schema.

mod client;
mod promise;
mod secrets;

use std::sync::Arc;

use napi::Env;
use napi::bindgen_prelude::{Buffer, Function, Object};
use napi_derive::napi;
use push_sdk::{Client, DatabaseKey, SECRET_KEYS, SdkError};
use push_sdk_ffi::json;
use serde_json::Value;

use crate::client::NativeClient;
use crate::promise::{Settle, failure};
use crate::secrets::{SecretArgs, SecretBridge};

/// The keys the SDK keeps in the app's secret store.
#[napi]
pub fn secret_keys() -> Vec<String> {
    SECRET_KEYS.iter().map(|key| (*key).to_owned()).collect()
}

/// Runs `work` on a thread of its own, settling the promise with its result.
fn on_thread<'env, T: napi::bindgen_prelude::ToNapiValue + Send + 'static>(
    env: &'env Env,
    name: &str,
    work: impl FnOnce() -> Result<T, SdkError> + Send + 'static,
) -> napi::Result<Object<'env>> {
    let (settle, promise) = Settle::new(env)?;
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || settle.finish(work()))
        .map_err(|_| failure(&SdkError::Invalid("cannot start a thread".to_owned())))?;
    Ok(promise)
}

/// Opens a client; resolves to a `NativeClient`. `config` is the JSON of
/// [`push_sdk_ffi::json::config`]; the database key comes apart, as bytes. `secret_call`
/// is the app's store, behind `index.js`'s bridge. The secrets are loaded and the client
/// opened on a thread of its own, never on the JavaScript thread.
#[napi(catch_unwind)]
pub fn open<'env>(
    env: &'env Env,
    config: Value,
    database_key: Option<Buffer>,
    secret_call: Function<'env, SecretArgs, ()>,
) -> napi::Result<Object<'env>> {
    let config = json::config(config, database_key.as_deref());
    let bridge = Arc::new(SecretBridge::new(&secret_call)?);
    on_thread(env, "push-sdk-open", move || {
        let config = config?;
        bridge.preload()?;
        Client::open(config, bridge).map(NativeClient::new)
    })
}

/// A database key derived from what the user knows and a salt the app keeps (Argon2id,
/// 19 MiB, 2 passes; tens of milliseconds, on a thread of its own); resolves to 32 bytes.
#[napi(catch_unwind)]
pub fn derive_database_key(env: &Env, secret: Buffer, salt: Buffer) -> napi::Result<Object<'_>> {
    let (secret, salt) = (secret.to_vec(), salt.to_vec());
    on_thread(env, "push-sdk-key", move || {
        DatabaseKey::derive(&secret, &salt).map(|key| Buffer::from(key.as_bytes().to_vec()))
    })
}

/// A new random salt for `deriveDatabaseKey`.
#[napi]
pub fn new_salt() -> Buffer {
    Buffer::from(DatabaseKey::new_salt().to_vec())
}

/// Hands the app's answer to a secret store call back to the SDK thread waiting for it.
#[napi(catch_unwind)]
pub fn complete_secret_call(id: u32, error: Option<String>, value: Option<Buffer>) {
    secrets::complete(id, error, value.map(|value| value.to_vec()));
}
```

Create `bindings/node/src/promise.rs`:

```rust
//! Promises settled from SDK threads.

use napi::bindgen_prelude::{Object, ToNapiValue};
use napi::{Env, JsDeferred, Status};
use push_sdk::SdkError;
use push_sdk_ffi::json;

type Resolver<T> = Box<dyn FnOnce(Env) -> napi::Result<T> + Send>;

/// A promise to settle from any thread. If it is dropped unsettled — the client closed
/// while the call ran, so its task was dropped — it rejects with `closed`: a promise never
/// stays pending, which would also keep Node's event loop alive.
pub struct Settle<T: ToNapiValue + Send + 'static> {
    deferred: Option<JsDeferred<T, Resolver<T>>>,
}

/// The rejection for `error`: an `Error` whose message is the error's JSON.
pub fn failure(error: &SdkError) -> napi::Error {
    napi::Error::new(Status::GenericFailure, json::error(error).to_string())
}

impl<T: ToNapiValue + Send + 'static> Settle<T> {
    pub fn new(env: &Env) -> napi::Result<(Self, Object<'_>)> {
        let (deferred, promise) = env.create_deferred()?;
        Ok((
            Self {
                deferred: Some(deferred),
            },
            promise,
        ))
    }

    pub fn finish(mut self, result: Result<T, SdkError>) {
        if let Some(deferred) = self.deferred.take() {
            match result {
                Ok(value) => deferred.resolve(Box::new(move |_| Ok(value))),
                Err(error) => deferred.reject(failure(&error)),
            }
        }
    }
}

impl<T: ToNapiValue + Send + 'static> Drop for Settle<T> {
    fn drop(&mut self) {
        if let Some(deferred) = self.deferred.take() {
            deferred.reject(failure(&SdkError::Closed));
        }
    }
}
```

Create `bindings/node/src/secrets.rs`:

```rust
//! The app's secret store, written in JavaScript, as the SDK's [`SecretStore`].
//!
//! The SDK calls its store with blocking calls on its own threads, while the JavaScript
//! store answers on the JavaScript thread, often asynchronously (a keychain module returns
//! promises). A blocking call into JavaScript from a thread the JavaScript thread waits for
//! would deadlock, so the bridge keeps two rules:
//!
//! - **Reads never reach JavaScript.** The three values are loaded once, before the client
//!   opens (on the opening thread, while the JavaScript thread is free), and kept in a
//!   cache. `load` answers from it, so the calls JavaScript makes synchronously
//!   (`account()`) never wait for JavaScript.
//! - **Writes wait for JavaScript, but never on the JavaScript thread.** `save` and
//!   `delete` post the call through a threadsafe function and wait, up to 30 s, for
//!   `completeSecretCall`; the cache changes only once the app confirmed the write, so the
//!   SDK's rule that credentials are saved before they are used still holds. Every call
//!   that can write (sign-in, refresh, sign-out) runs on an SDK thread and returns a promise,
//!   so the JavaScript thread is free to answer. A write on the JavaScript thread itself is
//!   refused rather than deadlocked.
//!
//! The app's store must not be changed behind the SDK's back while a client is open.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{LazyLock, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use napi::Status;
use napi::bindgen_prelude::{Buffer, FnArgs, Function};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use push_sdk::{SECRET_KEYS, SdkError, SecretError, SecretStore};

/// How long an SDK thread waits for the app's store to answer a write. A keychain may ask
/// the user first.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// What JavaScript receives: `(id, operation, key, value)`; it answers with
/// `completeSecretCall(id, error, value)`.
pub type SecretArgs = FnArgs<(u32, String, String, Option<Buffer>)>;

/// One call, as it waits in the threadsafe function's queue.
pub struct SecretCall {
    id: u32,
    operation: &'static str,
    key: String,
    value: Option<Vec<u8>>,
}

type Answer = Result<Option<Vec<u8>>, String>;

/// The SDK threads waiting for an answer, by call id.
static WAITING: LazyLock<Mutex<HashMap<u32, SyncSender<Answer>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// Wakes the SDK thread waiting for call `id`; an answer nobody waits for (it timed out) is
/// dropped.
pub fn complete(id: u32, error: Option<String>, value: Option<Vec<u8>>) {
    let waiting = WAITING.lock().map(|mut waiting| waiting.remove(&id));
    if let Ok(Some(sender)) = waiting {
        let _ = sender.send(match error {
            Some(error) => Err(error),
            None => Ok(value),
        });
    }
}

type Call = ThreadsafeFunction<SecretCall, (), SecretArgs, Status, false, true>;

/// The app's JavaScript store, with the values it holds.
pub struct SecretBridge {
    call: Call,
    cache: Mutex<HashMap<String, Vec<u8>>>,
    js_thread: ThreadId,
}

impl SecretBridge {
    /// Created on the JavaScript thread. The threadsafe function is weak: on its own it
    /// does not keep Node running.
    pub fn new(function: &Function<'_, SecretArgs, ()>) -> napi::Result<Self> {
        let call = function
            .build_threadsafe_function::<SecretCall>()
            .callee_handled::<false>()
            .weak::<true>()
            .build_callback(|context| {
                let call = context.value;
                Ok(FnArgs::from((
                    call.id,
                    call.operation.to_owned(),
                    call.key,
                    call.value.map(Buffer::from),
                )))
            })?;
        Ok(Self {
            call,
            cache: Mutex::new(HashMap::new()),
            js_thread: std::thread::current().id(),
        })
    }

    /// Loads every value the SDK may read into the cache. Called before the client opens,
    /// on the opening thread.
    pub fn preload(&self) -> Result<(), SdkError> {
        for key in SECRET_KEYS {
            let value = self
                .ask("load", key, None)
                .map_err(|error| SdkError::Secrets(error.0))?;
            if let Some(value) = value {
                self.cache().insert(key.to_owned(), value);
            }
        }
        Ok(())
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<u8>>> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Asks the JavaScript store and waits for its answer, off the JavaScript thread.
    fn ask(
        &self,
        operation: &'static str,
        key: &str,
        value: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, SecretError> {
        if std::thread::current().id() == self.js_thread {
            return Err(SecretError(
                "the secret store was called on the JavaScript thread".to_owned(),
            ));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (sender, answer) = sync_channel(1);
        if let Ok(mut waiting) = WAITING.lock() {
            waiting.insert(id, sender);
        }
        let call = SecretCall {
            id,
            operation,
            key: key.to_owned(),
            value: value.map(<[u8]>::to_vec),
        };
        if self
            .call
            .call(call, ThreadsafeFunctionCallMode::NonBlocking)
            != Status::Ok
        {
            complete(id, Some("Node.js is shutting down".to_owned()), None);
        }
        // On a runtime worker, the worker's other tasks move elsewhere while it waits.
        let answer = wait(|| answer.recv_timeout(ANSWER_TIMEOUT));
        match answer {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(SecretError(format!(
                "the app's secret store failed: {error}"
            ))),
            Err(_) => {
                complete(id, None, None);
                Err(SecretError(format!(
                    "the app's secret store did not answer within {} s",
                    ANSWER_TIMEOUT.as_secs()
                )))
            }
        }
    }
}

/// Runs a blocking wait; on a worker of the client's runtime, through `block_in_place`.
fn wait<T>(blocking: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) if runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(blocking)
        }
        _ => blocking(),
    }
}

impl SecretStore for SecretBridge {
    fn load(&self, key: &str) -> Result<Option<Vec<u8>>, SecretError> {
        Ok(self.cache().get(key).cloned())
    }

    fn save(&self, key: &str, value: &[u8]) -> Result<(), SecretError> {
        self.ask("save", key, Some(value))?;
        self.cache().insert(key.to_owned(), value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.ask("delete", key, None)?;
        self.cache().remove(key);
        Ok(())
    }
}
```

Create `bindings/node/test/client.test.js`:

```js
'use strict';

// The client's core through Node: opening, the secret store bridge, sign-in, the
// connection, events and errors, against push-sdk's fake auth-svc and gateway.

const { test, before, after } = require('node:test');
const assert = require('node:assert/strict');
const { execFileSync } = require('node:child_process');
const path = require('node:path');

const { Client, PushError, deriveDatabaseKey, newSalt, SECRET_KEYS } = require('..');
const { startFakes, memoryStore, eventQueue, scratchDir } = require('./support');

let fakes;
before(async () => {
  fakes = await startFakes();
});
after(() => fakes.stop());

const config = (extra = {}) => ({
  authUrl: fakes.authUrl,
  socketUrl: fakes.socketUrl,
  databasePath: ':memory:',
  ...extra,
});

const code = (expected) => (error) => {
  assert.ok(error instanceof PushError, `a PushError, not ${error}`);
  assert.equal(error.code, expected);
  return true;
};

test('signs in through an asynchronous store while the JavaScript thread keeps working', async () => {
  const store = memoryStore({ delayMs: 20 });
  const client = await Client.open(config(), store);
  assert.equal(client.account(), null);
  let synchronousCalls = 0;
  // While an SDK thread waits for the store's promises, the JavaScript thread runs the
  // client's synchronous calls: nothing waits for the other.
  const ticker = setInterval(() => {
    client.account();
    client.connectionState();
    synchronousCalls += 1;
  }, 1);
  const account = await client.signIn('a@example.com', 'password', 'node test', 'desktop');
  clearInterval(ticker);
  assert.equal(account.userId, fakes.userId);
  assert.deepEqual(client.account(), account);
  assert.ok(synchronousCalls > 0);
  assert.ok(store.values.has('push.session'), 'the session is in the app store');
  assert.ok(!store.values.has('push.registration'), 'the registration is done');
  await client.close();

  // Opened again, the client loads the session from the store before it answers.
  const again = await Client.open(config(), store);
  assert.deepEqual(again.account(), account);
  await again.close();
});

test('a store that fails a save fails the sign-in with code secrets, and a retry succeeds', async () => {
  for (const async of [false, true]) {
    const store = memoryStore();
    const client = await Client.open(config(), store);
    store.failNext = { key: 'push.session', async };
    await assert.rejects(
      client.signIn('a@example.com', 'password', 'node test', 'desktop'),
      (error) => code('secrets')(error) && error.retryable === false,
    );
    assert.ok(store.values.has('push.registration'), 'a retry names the same registration');
    const account = await client.signIn('a@example.com', 'password', 'node test', 'desktop');
    assert.equal(account.userId, fakes.userId);
    assert.ok(store.values.has('push.session'));
    await client.close();
  }
});

test('connects and reports the states as events, in order', async (t) => {
  const client = await Client.open(config(), memoryStore());
  // A listener keeps Node running until the client is closed, even when the test fails.
  t.after(() => client.close());
  const events = eventQueue(client);
  await client.signIn('a@example.com', 'password', 'node test', 'desktop');
  client.connect();
  assert.equal(client.connectionState().state, 'connecting');
  await events.next((event) => event.type === 'connectionStateChanged' && event.state === 'connected');
  await client.waitConnected(5000);
  const states = events.seen.filter((event) => event.type === 'connectionStateChanged').map((event) => event.state);
  assert.deepEqual(states.slice(0, 2), ['connecting', 'connected']);
  client.disconnect();
  await events.next((event) => event.type === 'connectionStateChanged' && event.state === 'offline');
  await client.close();
  await assert.rejects(client.signOut(), code('closed'));
  assert.throws(() => client.connect(), code('closed'));
});

test('errors carry the SDK codes', async () => {
  const client = await Client.open(config(), memoryStore());
  await assert.rejects(
    client.signIn('a@example.com', 'wrong password', 'node test', 'desktop'),
    (error) => code('invalid_credentials')(error) && error.retryable === false,
  );
  await assert.rejects(client.signIn('a@example.com', 'password', 'node test', 'toaster'), code('invalid'));
  await assert.rejects(client.waitConnected(50), code('offline'));
  await client.close();
  await assert.rejects(Client.open(config({ databaseKey: new Uint8Array(31) }), memoryStore()), code('invalid'));
  await assert.rejects(Client.open(config({ databaseFile: 'x' }), memoryStore()), code('invalid'));
  await assert.rejects(Client.open(config(), { load() {} }), code('invalid'));
});

test('an encrypted database opens only with its key', async () => {
  const salt = newSalt();
  assert.equal(salt.length, 16);
  const key = await deriveDatabaseKey('2468', salt);
  assert.equal(key.length, 32);
  assert.deepEqual(await deriveDatabaseKey('2468', salt), key);
  const wrong = await deriveDatabaseKey('1357', salt);
  const databasePath = path.join(scratchDir('key'), 'encrypted.db');
  const first = await Client.open(config({ databasePath, databaseKey: key }), memoryStore());
  await first.close();
  await assert.rejects(Client.open(config({ databasePath, databaseKey: wrong }), memoryStore()), code('wrong_key'));
  await assert.rejects(Client.open(config({ databasePath }), memoryStore()), code('wrong_key'));
  const again = await Client.open(config({ databasePath, databaseKey: key }), memoryStore());
  await again.changeDatabaseKey(wrong);
  await again.close();
  const changed = await Client.open(config({ databasePath, databaseKey: wrong }), memoryStore());
  await changed.close();
  assert.deepEqual([...SECRET_KEYS], ['push.session', 'push.device', 'push.registration']);
});

test('a closed client lets Node exit', () => {
  // A listener keeps Node running like an open socket; close() releases it.
  const script = `
    const { Client } = require(${JSON.stringify(path.join(__dirname, '..'))});
    const values = new Map();
    const store = { load: (k) => values.get(k), save: (k, v) => { values.set(k, v); }, delete: (k) => { values.delete(k); } };
    (async () => {
      const client = await Client.open({ authUrl: ${JSON.stringify(fakes.authUrl)}, socketUrl: ${JSON.stringify(fakes.socketUrl)}, databasePath: ':memory:' }, store);
      client.onEvent(() => {});
      await client.signIn('a@example.com', 'password', 'exit test', 'desktop');
      client.connect();
      await client.waitConnected(5000);
      await client.close();
      console.log('closed');
    })();
  `;
  const output = execFileSync(process.execPath, ['-e', script], { timeout: 15000, encoding: 'utf8' });
  assert.match(output, /closed/);
});
```

Create `bindings/node/test/support.js`:

```js
'use strict';

// What the tests share: the fake servers, an in-memory secret store and an event queue.

const { spawn } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const readline = require('node:readline');

const targetDir = process.env.CARGO_TARGET_DIR || path.join(__dirname, '..', '..', '..', 'target');

/**
 * Starts push-sdk-fakes (built by `npm test`): a fake auth-svc and a gateway that answers by
 * itself. Resolves to their URLs and the identity they sign everyone in as.
 */
function startFakes() {
  const program = path.join(targetDir, 'debug', process.platform === 'win32' ? 'push-sdk-fakes.exe' : 'push-sdk-fakes');
  const child = spawn(program, [], { stdio: ['pipe', 'pipe', 'inherit'] });
  return new Promise((resolve, reject) => {
    child.once('error', reject);
    readline.createInterface({ input: child.stdout }).once('line', (line) => {
      const fakes = JSON.parse(line);
      // Closing its input ends it.
      fakes.stop = () => child.stdin.end();
      resolve(fakes);
    });
  });
}

/**
 * A secret store in memory, answering asynchronously after `delayMs`, as a keychain module
 * does. `failNext` makes the next save of a key throw (synchronously, or with `async: true`
 * reject).
 */
function memoryStore({ delayMs = 0 } = {}) {
  const store = {
    values: new Map(),
    calls: [],
    failNext: null,
    later: () => new Promise((resolve) => setTimeout(resolve, delayMs)),
    async load(key) {
      store.calls.push({ op: 'load', key });
      await store.later();
      return store.values.get(key) ?? null;
    },
    save(key, value) {
      store.calls.push({ op: 'save', key });
      if (store.failNext && store.failNext.key === key) {
        const failure = store.failNext;
        store.failNext = null;
        if (!failure.async) throw new Error('the keychain is locked');
        return store.later().then(() => Promise.reject(new Error('the keychain is locked')));
      }
      return store.later().then(() => {
        store.values.set(key, Buffer.from(value));
      });
    },
    async delete(key) {
      store.calls.push({ op: 'delete', key });
      await store.later();
      store.values.delete(key);
    },
  };
  return store;
}

/** Collects a client's events; `next(match)` resolves to the first one `match` accepts. */
function eventQueue(client) {
  const seen = [];
  const waiting = [];
  client.onEvent((event) => {
    seen.push(event);
    for (const waiter of [...waiting]) {
      if (waiter.match(event)) {
        waiting.splice(waiting.indexOf(waiter), 1);
        waiter.resolve(event);
      }
    }
  });
  return {
    seen,
    next(match, timeoutMs = 10000) {
      const found = seen.find(match);
      if (found) return Promise.resolve(found);
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('no such event in time')), timeoutMs);
        waiting.push({ match, resolve: (event) => { clearTimeout(timer); resolve(event); } });
      });
    },
  };
}

/** A directory of its own for database files, under cargo's target directory. */
function scratchDir(name) {
  const dir = path.join(targetDir, 'tmp', 'push-sdk-node', name);
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { recursive: true });
  return dir;
}

module.exports = { startFakes, memoryStore, eventQueue, scratchDir };
```

- [ ] **Step 2: Red — reads that go to JavaScript**

In `bindings/node/src/secrets.rs`, `SecretStore::load`, replace
`Ok(self.cache().get(key).cloned())` with `self.ask("load", key, None)` and run:

```powershell
cd bindings/node
npm test
cd ../..
```

Expected: `not ok 1 - signs in through an asynchronous store while the JavaScript thread
keeps working` — `account()` on the JavaScript thread now needs the store and fails with
`secrets` ("the secret store was called on the JavaScript thread") instead of
deadlocking. Restore the line.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
cd bindings/node
npm test
cd ../..
git add -A Cargo.toml Cargo.lock bindings
git commit -m "push-sdk-node: the Node binding: promises from the client's runtime, events through a threadsafe function, the secret store bridged from JavaScript"
```

Expected: 977 tests pass, no warnings, no diff; `npm test` ends `# pass 6`,
`# fail 0`.

---

### Task 6: The Node calls for data, sending and requests; typings checked

**Files:**
- Modify: `bindings/node/src/client.rs`, `bindings/node/index.js`,
  `bindings/node/package.json`
- Create: `bindings/node/index.d.ts`, `bindings/node/package-lock.json`, `bindings/node/tsconfig.json`,
  `bindings/node/test/messaging.test.js`, `bindings/node/test/types.ts`

- [ ] **Step 1: Write the code**

Create `bindings/node/index.d.ts`:

```ts
// Types of push-sdk for Node.js (index.js). docs/sdk-bindings.md describes every call.
// Ids are UUID strings; times are microseconds since the Unix epoch.

/** The servers and the database of one app profile. */
export interface Config {
  /** auth-svc's public URL: `https://push.example`. */
  authUrl: string;
  /** The gateway's WebSocket: `wss://push.example/v1/socket`. */
  socketUrl: string;
  /** The SQLite file of this profile; `:memory:` keeps nothing. */
  databasePath: string;
  /** The `Host` header, when the URLs name an address rather than the virtual host. */
  host?: string;
  /** How long a request waits for the connection, and then for its answer (15 000). */
  requestTimeoutMs?: number;
  /** 32 bytes that encrypt the whole database; never stored or logged by the SDK. */
  databaseKey?: Uint8Array;
}

/**
 * The app's secret store: a few small values under `SECRET_KEYS`. Methods may return
 * promises. Loaded once when the client opens, then written through.
 */
export interface SecretStore {
  load(key: string): Uint8Array | null | undefined | Promise<Uint8Array | null | undefined>;
  save(key: string, value: Uint8Array): void | Promise<void>;
  delete(key: string): void | Promise<void>;
}

export type Platform = 'ios' | 'android' | 'web' | 'desktop' | 'other';

export interface Account {
  userId: string;
  deviceId: string;
}

export type ConnectionState =
  | { state: 'offline' | 'connecting' | 'connected' | 'signedOut' | 'deviceRemoved' | 'openedElsewhere' }
  | { state: 'reconnecting'; attempt: number; retryInMs: number };

export interface Message {
  conversationId: string;
  /** Chosen by the sending device; the handle for edit, delete and cancel. */
  clientMsgId: string;
  messageId: string | null;
  seq: number | null;
  senderId: string;
  kind: 'text' | 'system';
  /** null once deleted. */
  body: string | null;
  clientTs: number | null;
  serverTs: number | null;
  editedAt: number | null;
  deleted: boolean;
  status: 'pending' | 'sent' | 'delivered' | 'read' | 'received';
}

export interface Conversation {
  id: string;
  kind: 'direct' | 'group' | 'unknown';
  peer: string | null;
  title: string | null;
  lastSeq: number;
  readSeq: number;
  unread: number;
  peerDeliveredSeq: number;
  peerReadSeq: number;
  updatedAt: number;
}

export interface Contact {
  userId: string;
  state: 'none' | 'requestedOut' | 'requestedIn' | 'accepted' | 'blocked';
}

export interface Invite {
  code: string;
  expiresAt: number;
  maxUses: number;
  uses: number;
}

export interface Profile {
  userId: string;
  handle: string;
  displayName: string;
}

export interface Device {
  deviceId: string;
  name: string;
  platform: string;
  createdAt: string;
  lastSeenAt: string | null;
  current: boolean;
}

export interface ErrorDetail {
  code: string;
  message: string;
  retryable: boolean;
  retryAfterS: number | null;
}

export type PushEvent =
  | ({ type: 'connectionStateChanged' } & ConnectionState)
  | { type: 'messageAdded' | 'messageSent' | 'messageEdited' | 'messageDeleted'; message: Message }
  | {
      type: 'sendFailed';
      conversationId: string;
      clientMsgId: string;
      kind: 'send' | 'edit' | 'delete';
      error: ErrorDetail;
    }
  | { type: 'receiptChanged'; conversationId: string; peerDeliveredSeq: number; peerReadSeq: number }
  | { type: 'conversationChanged'; conversation: Conversation }
  | { type: 'contactChanged'; contact: Contact }
  | { type: 'signedOut' | 'deviceRemoved' | 'openedElsewhere' | 'eventsLost' };

/** A failed call; `code` is the SDK's stable error code, as in Rust and C. */
export class PushError extends Error {
  readonly code: string;
  readonly retryable: boolean;
  readonly retryAfterS: number | null;
}

export class Client {
  private constructor();
  static open(config: Config, secretStore: SecretStore): Promise<Client>;
  onEvent(listener: (event: PushEvent) => void): void;
  close(): Promise<void>;

  signUp(email: string, password: string, handle: string, displayName: string): Promise<void>;
  resendVerification(email: string): Promise<void>;
  forgotPassword(email: string): Promise<void>;
  signIn(email: string, password: string, deviceName: string, platform: Platform): Promise<Account>;
  account(): Account | null;
  signOut(): Promise<void>;

  connect(): void;
  disconnect(): void;
  connectionState(): ConnectionState;
  waitConnected(timeoutMs: number): Promise<void>;
  changeDatabaseKey(databaseKey: Uint8Array | null): Promise<void>;

  conversations(): Promise<Conversation[]>;
  conversation(conversationId: string): Promise<Conversation | null>;
  messages(conversationId: string, beforeSeq: number | null, limit: number): Promise<Message[]>;
  contacts(): Promise<Contact[]>;

  send(conversationId: string, text: string): Promise<Message>;
  edit(conversationId: string, clientMsgId: string, text: string): Promise<string>;
  delete(conversationId: string, clientMsgId: string): Promise<string>;
  cancel(conversationId: string, clientMsgId: string): Promise<boolean>;
  markRead(conversationId: string, seq: number): Promise<void>;

  listConversations(): Promise<Conversation[]>;
  fetchHistory(conversationId: string, beforeSeq: number | null, limit: number): Promise<Message[]>;
  openDirect(userId: string): Promise<Conversation>;
  listContacts(): Promise<Contact[]>;
  requestContact(userId: string): Promise<Contact>;
  acceptContact(userId: string): Promise<Contact>;
  declineContact(userId: string): Promise<Contact>;
  block(userId: string): Promise<Contact>;
  unblock(userId: string): Promise<Contact>;
  createInvite(expiresInS: number, maxUses: number): Promise<Invite>;
  redeemInvite(code: string): Promise<string>;
  revokeInvite(code: string): Promise<void>;
  profiles(userIds: string[]): Promise<Profile[]>;
  lookup(handle: string): Promise<Profile>;
  devices(): Promise<Device[]>;
  revokeDevice(deviceId: string): Promise<void>;
}

export function deriveDatabaseKey(secret: Uint8Array | string, salt: Uint8Array): Promise<Uint8Array>;
export function newSalt(): Uint8Array;
export const SECRET_KEYS: readonly string[];
```

Change `bindings/node/index.js` (save the block to a file and `git apply` it):

```diff
diff --git a/bindings/node/index.js b/bindings/node/index.js
index db3aa6e..cc325c7 100644
--- a/bindings/node/index.js
+++ b/bindings/node/index.js
@@ -195,6 +195,122 @@ class Client {
     return settled(() => this.#native.changeDatabaseKey(key));
   }
 
+  // ----- local data -----
+
+  /** Every conversation this device knows, most recently changed first. */
+  conversations() {
+    return settled(() => this.#native.conversations());
+  }
+
+  /** One conversation, or null. */
+  conversation(conversationId) {
+    return settled(() => this.#native.conversation(conversationId));
+  }
+
+  /**
+   * Up to `limit` stored messages before `beforeSeq` (null: the newest, then those still in
+   * the outbox), oldest first.
+   */
+  messages(conversationId, beforeSeq, limit) {
+    return settled(() => this.#native.messages(conversationId, beforeSeq ?? null, limit));
+  }
+
+  contacts() {
+    return settled(() => this.#native.contacts());
+  }
+
+  // ----- sending -----
+
+  /** Queues a message; resolves to the local echo, whose `clientMsgId` is its handle. */
+  send(conversationId, text) {
+    return settled(() => this.#native.send(conversationId, text));
+  }
+
+  /** Resolves to the change's own id. */
+  edit(conversationId, clientMsgId, text) {
+    return settled(() => this.#native.edit(conversationId, clientMsgId, text));
+  }
+
+  /** Resolves to the change's own id. */
+  delete(conversationId, clientMsgId) {
+    return settled(() => this.#native.delete(conversationId, clientMsgId));
+  }
+
+  /** Takes back a message never sent; resolves to whether it was. */
+  cancel(conversationId, clientMsgId) {
+    return settled(() => this.#native.cancel(conversationId, clientMsgId));
+  }
+
+  markRead(conversationId, seq) {
+    return settled(() => this.#native.markRead(conversationId, seq));
+  }
+
+  // ----- requests -----
+
+  listConversations() {
+    return settled(() => this.#native.listConversations());
+  }
+
+  fetchHistory(conversationId, beforeSeq, limit) {
+    return settled(() => this.#native.fetchHistory(conversationId, beforeSeq ?? null, limit));
+  }
+
+  openDirect(userId) {
+    return settled(() => this.#native.openDirect(userId));
+  }
+
+  listContacts() {
+    return settled(() => this.#native.listContacts());
+  }
+
+  requestContact(userId) {
+    return settled(() => this.#native.changeContact('request', userId));
+  }
+
+  acceptContact(userId) {
+    return settled(() => this.#native.changeContact('accept', userId));
+  }
+
+  declineContact(userId) {
+    return settled(() => this.#native.changeContact('decline', userId));
+  }
+
+  block(userId) {
+    return settled(() => this.#native.changeContact('block', userId));
+  }
+
+  unblock(userId) {
+    return settled(() => this.#native.changeContact('unblock', userId));
+  }
+
+  createInvite(expiresInS, maxUses) {
+    return settled(() => this.#native.createInvite(expiresInS, maxUses));
+  }
+
+  /** Resolves to the inviter's user id. */
+  redeemInvite(code) {
+    return settled(() => this.#native.redeemInvite(code));
+  }
+
+  revokeInvite(code) {
+    return settled(() => this.#native.revokeInvite(code));
+  }
+
+  profiles(userIds) {
+    return settled(() => this.#native.profiles(userIds));
+  }
+
+  lookup(handle) {
+    return settled(() => this.#native.lookup(handle));
+  }
+
+  devices() {
+    return settled(() => this.#native.devices());
+  }
+
+  revokeDevice(deviceId) {
+    return settled(() => this.#native.revokeDevice(deviceId));
+  }
 }
 
 /** Derives a 32-byte database key from what the user knows and a salt the app keeps. */
```

Create `bindings/node/package-lock.json`:

```json
{
  "name": "push-sdk",
  "version": "0.1.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "push-sdk",
      "version": "0.1.0",
      "license": "MIT",
      "devDependencies": {
        "typescript": "5.9.3"
      },
      "engines": {
        "node": ">=18"
      }
    },
    "node_modules/typescript": {
      "version": "5.9.3",
      "resolved": "https://registry.npmjs.org/typescript/-/typescript-5.9.3.tgz",
      "integrity": "sha512-jl1vZzPDinLr9eUt3J/t7V6FgNEw9QjvBPdysz9KfQDD41fQrC2Y4vKQdiaUpFT4bXlb1RHhLpp8wtm6M5TgSw==",
      "dev": true,
      "license": "Apache-2.0",
      "bin": {
        "tsc": "bin/tsc",
        "tsserver": "bin/tsserver"
      },
      "engines": {
        "node": ">=14.17"
      }
    }
  }
}
```

Change `bindings/node/package.json` (save the block to a file and `git apply` it):

```diff
diff --git a/bindings/node/package.json b/bindings/node/package.json
index a4a1126..dd6989e 100644
--- a/bindings/node/package.json
+++ b/bindings/node/package.json
@@ -5,8 +5,10 @@
   "private": true,
   "license": "MIT",
   "main": "index.js",
+  "types": "index.d.ts",
   "files": [
     "index.js",
+    "index.d.ts",
     "push_sdk.node"
   ],
   "engines": {
@@ -15,6 +17,10 @@
   "scripts": {
     "build": "node scripts/build.js",
     "pretest": "node scripts/build.js --with-fakes",
-    "test": "node --test test/client.test.js"
+    "test": "node --test test/client.test.js test/messaging.test.js",
+    "typecheck": "tsc -p tsconfig.json"
+  },
+  "devDependencies": {
+    "typescript": "5.9.3"
   }
 }
```

Change `bindings/node/src/client.rs` (save the block to a file and `git apply` it):

```diff
diff --git a/bindings/node/src/client.rs b/bindings/node/src/client.rs
index 2155a09..59e8d76 100644
--- a/bindings/node/src/client.rs
+++ b/bindings/node/src/client.rs
@@ -7,7 +7,9 @@ use napi::bindgen_prelude::{Buffer, Function, Object};
 use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
 use napi::{Env, Status};
 use napi_derive::napi;
-use push_sdk::{Client, DatabaseKey, Events, SdkError};
+use push_sdk::{
+    Client, ClientMsgId, ConversationId, DatabaseKey, DeviceId, Events, SdkError, UserId,
+};
 use push_sdk_ffi::{block_on, json};
 use serde_json::Value;
 
@@ -29,6 +31,14 @@ fn invalid(message: &str) -> napi::Error {
     failure(&SdkError::Invalid(message.to_owned()))
 }
 
+fn conversation_id(text: &str) -> Result<ConversationId, SdkError> {
+    json::id(text, "conversationId")
+}
+
+fn user_id(text: &str) -> Result<UserId, SdkError> {
+    json::id(text, "userId")
+}
+
 impl NativeClient {
     pub fn new((client, events): (Client, Events)) -> Self {
         Self {
@@ -250,4 +260,252 @@ impl NativeClient {
                 .map(|()| Value::Null)
         })
     }
+
+    // ----- local data -----
+
+    #[napi(catch_unwind)]
+    pub fn conversations<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            Ok(json::list(&client.conversations()?, json::conversation))
+        })
+    }
+
+    /// One conversation, or null.
+    #[napi(catch_unwind)]
+    pub fn conversation<'env>(&self, env: &'env Env, id: String) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            let found = client.conversation(conversation_id(&id)?)?;
+            Ok(found.as_ref().map_or(Value::Null, json::conversation))
+        })
+    }
+
+    #[napi(catch_unwind)]
+    pub fn messages<'env>(
+        &self,
+        env: &'env Env,
+        conversation: String,
+        before_seq: Option<i64>,
+        limit: u32,
+    ) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            let messages = client.messages(conversation_id(&conversation)?, before_seq, limit)?;
+            Ok(json::list(&messages, json::message))
+        })
+    }
+
+    #[napi(catch_unwind)]
+    pub fn contacts<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            Ok(json::list(&client.contacts()?, json::contact))
+        })
+    }
+
+    // ----- sending -----
+
+    /// Queues a message; resolves to the local echo.
+    #[napi(catch_unwind)]
+    pub fn send<'env>(
+        &self,
+        env: &'env Env,
+        conversation: String,
+        text: String,
+    ) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            let message = client.send(conversation_id(&conversation)?, &text)?;
+            Ok(json::message(&message))
+        })
+    }
+
+    /// Resolves to the change's id.
+    #[napi(catch_unwind)]
+    pub fn edit<'env>(
+        &self,
+        env: &'env Env,
+        conversation: String,
+        target: String,
+        text: String,
+    ) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            let target: ClientMsgId = json::id(&target, "clientMsgId")?;
+            let change = client.edit(conversation_id(&conversation)?, target, &text)?;
+            Ok(Value::String(change.to_string()))
+        })
+    }
+
+    /// Resolves to the change's id.
+    #[napi(catch_unwind)]
+    pub fn delete<'env>(
+        &self,
+        env: &'env Env,
+        conversation: String,
+        target: String,
+    ) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            let target: ClientMsgId = json::id(&target, "clientMsgId")?;
+            let change = client.delete(conversation_id(&conversation)?, target)?;
+            Ok(Value::String(change.to_string()))
+        })
+    }
+
+    /// Resolves to whether the message was taken back.
+    #[napi(catch_unwind)]
+    pub fn cancel<'env>(
+        &self,
+        env: &'env Env,
+        conversation: String,
+        message: String,
+    ) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            let message: ClientMsgId = json::id(&message, "clientMsgId")?;
+            Ok(Value::Bool(
+                client.cancel(conversation_id(&conversation)?, message)?,
+            ))
+        })
+    }
+
+    #[napi(catch_unwind)]
+    pub fn mark_read<'env>(
+        &self,
+        env: &'env Env,
+        conversation: String,
+        seq: i64,
+    ) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            client
+                .mark_read(conversation_id(&conversation)?, seq)
+                .map(|()| Value::Null)
+        })
+    }
+
+    // ----- requests -----
+
+    #[napi(catch_unwind)]
+    pub fn list_conversations<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            Ok(json::list(
+                &client.list_conversations().await?,
+                json::conversation,
+            ))
+        })
+    }
+
+    #[napi(catch_unwind)]
+    pub fn fetch_history<'env>(
+        &self,
+        env: &'env Env,
+        conversation: String,
+        before_seq: Option<i64>,
+        limit: u32,
+    ) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            let id = conversation_id(&conversation)?;
+            let messages = client.fetch_history(id, before_seq, limit).await?;
+            Ok(json::list(&messages, json::message))
+        })
+    }
+
+    #[napi(catch_unwind)]
+    pub fn open_direct<'env>(&self, env: &'env Env, peer: String) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            let conversation = client.open_direct(user_id(&peer)?).await?;
+            Ok(json::conversation(&conversation))
+        })
+    }
+
+    #[napi(catch_unwind)]
+    pub fn list_contacts<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            Ok(json::list(&client.list_contacts().await?, json::contact))
+        })
+    }
+
+    /// `request`, `accept`, `decline`, `block` or `unblock`; resolves to the relationship.
+    #[napi(catch_unwind)]
+    pub fn change_contact<'env>(
+        &self,
+        env: &'env Env,
+        change: String,
+        user: String,
+    ) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            let user = user_id(&user)?;
+            let contact = match change.as_str() {
+                "request" => client.request_contact(user).await?,
+                "accept" => client.accept_contact(user).await?,
+                "decline" => client.decline_contact(user).await?,
+                "block" => client.block(user).await?,
+                "unblock" => client.unblock(user).await?,
+                _ => return Err(SdkError::Invalid(format!("no contact change {change:?}"))),
+            };
+            Ok(json::contact(&contact))
+        })
+    }
+
+    #[napi(catch_unwind)]
+    pub fn create_invite<'env>(
+        &self,
+        env: &'env Env,
+        expires_in_s: u32,
+        max_uses: u32,
+    ) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            Ok(json::invite(
+                &client.create_invite(expires_in_s, max_uses).await?,
+            ))
+        })
+    }
+
+    /// Resolves to the inviter's user id.
+    #[napi(catch_unwind)]
+    pub fn redeem_invite<'env>(&self, env: &'env Env, code: String) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            Ok(Value::String(
+                client.redeem_invite(&code).await?.to_string(),
+            ))
+        })
+    }
+
+    #[napi(catch_unwind)]
+    pub fn revoke_invite<'env>(&self, env: &'env Env, code: String) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            client.revoke_invite(&code).await.map(|()| Value::Null)
+        })
+    }
+
+    #[napi(catch_unwind)]
+    pub fn profiles<'env>(&self, env: &'env Env, ids: Vec<String>) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            let ids = ids
+                .iter()
+                .map(|id| user_id(id))
+                .collect::<Result<Vec<_>, _>>()?;
+            Ok(json::list(&client.profiles(ids).await?, json::profile))
+        })
+    }
+
+    #[napi(catch_unwind)]
+    pub fn lookup<'env>(&self, env: &'env Env, handle: String) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            Ok(json::profile(&client.lookup(&handle).await?))
+        })
+    }
+
+    #[napi(catch_unwind)]
+    pub fn devices<'env>(&self, env: &'env Env) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            Ok(json::list(&client.devices().await?, json::device))
+        })
+    }
+
+    #[napi(catch_unwind)]
+    pub fn revoke_device<'env>(
+        &self,
+        env: &'env Env,
+        device: String,
+    ) -> napi::Result<Object<'env>> {
+        self.promise(env, |client| async move {
+            let device: DeviceId = json::id(&device, "deviceId")?;
+            client.revoke_device(device).await.map(|()| Value::Null)
+        })
+    }
 }
```

Create `bindings/node/test/messaging.test.js`:

```js
'use strict';

// Messages, local data and requests through Node, against push-sdk's fake auth-svc and a
// gateway that stores every send and answers it with the peer's copy.

const { test, before, after } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { Client, PushError } = require('..');
const { startFakes, memoryStore, eventQueue } = require('./support');

let fakes;
before(async () => {
  fakes = await startFakes();
});
after(() => fakes.stop());

const PEER = '0199e1c2-0000-7000-8000-000000000001';

/** A client signed in and connected, closed after the test (a listener keeps Node running). */
async function connected(t) {
  const client = await Client.open(
    { authUrl: fakes.authUrl, socketUrl: fakes.socketUrl, databasePath: ':memory:' },
    memoryStore(),
  );
  t.after(() => client.close());
  const events = eventQueue(client);
  await client.signIn('a@example.com', 'password', 'node test', 'desktop');
  client.connect();
  await client.waitConnected(5000);
  return { client, events };
}

const code = (expected) => (error) => {
  assert.ok(error instanceof PushError, `a PushError, not ${error}`);
  assert.equal(error.code, expected);
  return true;
};

test('sends a message: the local echo, messageSent, the answer, the stored history', async (t) => {
  const { client, events } = await connected(t);
  const conversation = await client.openDirect(PEER);
  assert.equal(conversation.kind, 'direct');
  assert.equal(conversation.peer, PEER);
  const echo = await client.send(conversation.id, 'hello from Node');
  assert.equal(echo.status, 'pending');
  assert.equal(echo.body, 'hello from Node');
  const sent = await events.next((event) => event.type === 'messageSent' && event.message.clientMsgId === echo.clientMsgId);
  assert.equal(sent.message.seq, 1);
  const answer = await events.next((event) => event.type === 'messageAdded' && event.message.senderId === PEER);
  assert.equal(answer.message.body, 'hello from Node');
  assert.equal(answer.message.status, 'received');
  const history = await client.messages(conversation.id, null, 10);
  assert.deepEqual(history.map((message) => message.seq), [1, 2]);
  assert.equal(history[0].status, 'sent');
  assert.equal((await client.conversation(conversation.id)).lastSeq, 2);
  assert.equal((await client.conversations()).length, 1);

  const edit = await client.edit(conversation.id, echo.clientMsgId, 'hello again');
  assert.match(edit, /^[0-9a-f-]{36}$/);
  await events.next((event) => event.type === 'messageEdited' && event.message.clientMsgId === echo.clientMsgId);
  await client.delete(conversation.id, echo.clientMsgId);
  const deleted = await events.next((event) => event.type === 'messageDeleted');
  assert.equal(deleted.message.body, null);
  await client.markRead(conversation.id, 2);
  await client.close();
});

test('a message never sent can be taken back', async (t) => {
  const { client } = await connected(t);
  const conversation = await client.openDirect(PEER);
  client.disconnect();
  const queued = await client.send(conversation.id, 'never mind');
  assert.equal(await client.cancel(conversation.id, queued.clientMsgId), true);
  assert.deepEqual(await client.messages(conversation.id, null, 10), []);
  await client.close();
});

test('requests: invites and contacts, and their errors', async (t) => {
  const { client } = await connected(t);
  const invite = await client.createInvite(3600, 1);
  assert.equal(invite.code, 'ABCDEFGHJK');
  assert.equal(invite.maxUses, 1);
  assert.match(await client.redeemInvite('ABCDEFGHJK'), /^[0-9a-f-]{36}$/);
  await assert.rejects(client.redeemInvite('NOSUCHCODE'), (error) => code('invite_not_found')(error) && !error.retryable);
  await assert.rejects(client.revokeInvite('ABCDEFGHJK'), code('rejected'));
  assert.deepEqual(await client.listContacts(), []);
  assert.deepEqual(await client.listConversations(), []);
  await assert.rejects(client.send('0199e1c2-0000-7000-8000-0000000000ff', 'x'), code('invalid'));
  await assert.rejects(client.send('not an id', 'x'), code('invalid'));
  await assert.rejects(client.openDirect(42), code('invalid'));
  client.disconnect();
  await assert.rejects(client.listContacts(), (error) => code('offline')(error) && error.retryable);
  await client.close();
});

test('the typings declare every call of the client, and only those', () => {
  const declared = fs.readFileSync(path.join(__dirname, '..', 'index.d.ts'), 'utf8');
  const body = declared.slice(declared.indexOf('export class Client {'));
  const typed = new Set([...body.slice(0, body.indexOf('\n}')).matchAll(/^ {2}(?:static )?(\w+)\(/gm)].map((match) => match[1]));
  typed.delete('constructor');
  const implemented = new Set(
    Object.getOwnPropertyNames(Client.prototype).filter((name) => name !== 'constructor').concat(['open']),
  );
  assert.deepEqual([...typed].sort(), [...implemented].sort());
});
```

Create `bindings/node/test/types.ts`:

```ts
// Compiled by `npm run typecheck` (never run): the typings describe a real app's use.

import { Client, PushError, PushEvent, SecretStore, deriveDatabaseKey, newSalt, SECRET_KEYS } from '../index';

const values = new Map<string, Uint8Array>();
const store: SecretStore = {
  load: async (key) => values.get(key) ?? null,
  save: (key, value) => {
    values.set(key, value);
  },
  delete: async (key) => {
    values.delete(key);
  },
};

async function app(): Promise<void> {
  const databaseKey = await deriveDatabaseKey('2468', newSalt());
  const client = await Client.open(
    { authUrl: 'https://push.example', socketUrl: 'wss://push.example/v1/socket', databasePath: 'app.db', databaseKey },
    store,
  );
  client.onEvent((event: PushEvent) => {
    switch (event.type) {
      case 'messageAdded':
        console.log(event.message.clientMsgId, event.message.status);
        break;
      case 'connectionStateChanged':
        if (event.state === 'reconnecting') console.log(event.retryInMs);
        break;
      case 'sendFailed':
        console.log(event.error.code);
        break;
    }
  });
  const account = await client.signIn('a@example.com', 'password', 'laptop', 'desktop');
  console.log(account.userId, client.account()?.deviceId, SECRET_KEYS.length);
  client.connect();
  const conversation = await client.openDirect('0199e1c2-0000-7000-8000-000000000001');
  const echo = await client.send(conversation.id, 'hi');
  const history = await client.messages(conversation.id, null, 50);
  console.log(echo.seq, history.length);
  try {
    await client.redeemInvite('NOSUCHCODE');
  } catch (error) {
    if (error instanceof PushError && error.code === 'invite_not_found') console.log(error.retryable);
  }
  await client.close();
}

void app;
```

Create `bindings/node/tsconfig.json`:

```json
{
  "compilerOptions": {
    "strict": true,
    "noEmit": true,
    "target": "es2022",
    "module": "commonjs",
    "types": []
  },
  "files": ["index.d.ts", "test/types.ts"]
}
```

- [ ] **Step 2: Red — history read from the wrong place**

In `bindings/node/index.js`, `messages`, replace `beforeSeq ?? null` with `beforeSeq ?? 0`
and run `npm test` in `bindings/node`. Expected: `not ok 7 - sends a message: the local
echo, messageSent, the answer, the stored history` (`[]` instead of `[1, 2]`). Restore
the line.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
cd bindings/node
npm ci
npm test
npm run typecheck
cd ../..
git add -A bindings
git commit -m "push-sdk-node: the local data, sending and the requests; typings checked with tsc"
```

Expected: 977 tests pass, no warnings, no diff; `npm test` ends `# pass 10`,
`# fail 0`; `tsc` prints nothing.

---

### Task 7: The guide, the README and the CI job

**Files:**
- Create: `docs/sdk-bindings.md`
- Modify: `bindings/ffi/src/json.rs`, `README.md`, `docs/sdk-secure-storage.md`,
  `.github/workflows/ci.yml`

- [ ] **Step 1: The test first**

Apply only the `bindings/ffi/src/json.rs` patch of Step 2 (a test that the guide names every
error code with its retryability and every event type) and run
`cargo test -p push-sdk-ffi --locked`. Expected: it does not compile — `error: couldn't read`
`bindings/ffi/src/../../../docs/sdk-bindings.md` (the guide does not exist yet).

- [ ] **Step 2: Write the guide and the rest**

Change `.github/workflows/ci.yml` (save the block to a file and `git apply` it):

```diff
diff --git a/.github/workflows/ci.yml b/.github/workflows/ci.yml
index a27a885..15bc565 100644
--- a/.github/workflows/ci.yml
+++ b/.github/workflows/ci.yml
@@ -36,6 +36,28 @@ jobs:
       - run: docker pull redis:7-alpine
       - run: cargo test --workspace --locked
 
+  # The C and Node bindings on Linux: the generated header is current, a C program built
+  # with gcc runs against the shared library, and the Node tests and typings pass.
+  bindings:
+    runs-on: ubuntu-latest
+    timeout-minutes: 30
+    steps:
+      - uses: actions/checkout@v4
+      - uses: dtolnay/rust-toolchain@stable
+      - uses: Swatinem/rust-cache@v2
+      - uses: actions/setup-node@v4
+        with:
+          node-version: 22
+          cache: npm
+          cache-dependency-path: bindings/node/package-lock.json
+      - run: cargo test --locked -p push-sdk-c -p push-sdk-ffi
+      - run: npm ci
+        working-directory: bindings/node
+      - run: npm test
+        working-directory: bindings/node
+      - run: npm run typecheck
+        working-directory: bindings/node
+
   # The container images build (nothing is pushed). Deploying them is verified on
   # minikube before merge: deploy/minikube/up.ps1 -Smoke.
   images:
@@ -58,4 +80,4 @@ jobs:
       - uses: azure/setup-helm@v4
         with:
           version: v3.16.3
-      - run: pwsh deploy/helm/test-chart.ps1
\ No newline at end of file
+      - run: pwsh deploy/helm/test-chart.ps1
```

Change `README.md` (save the block to a file and `git apply` it):

```diff
diff --git a/README.md b/README.md
index 4db8873..fd114c7 100644
--- a/README.md
+++ b/README.md
@@ -19,8 +19,8 @@ one).
 
 ## The client SDK
 
-`crates/push-sdk` is the Rust core that apps embed (the Node and C bindings come with
-Plan 10). A `Client` signs up and in through auth-svc, keeps the device's credentials in
+`crates/push-sdk` is the Rust core that apps embed, from Rust or through its Node.js and C
+bindings. A `Client` signs up and in through auth-svc, keeps the device's credentials in
 a `SecretStore` the app provides (the platform's keychain), and holds the WebSocket to the
 gateway, reconnecting with backoff; it signs out only when the server says so (4001) and
 stops only when the device is removed (4003) or its data was opened elsewhere (4007).
@@ -45,6 +45,13 @@ stores the key: [docs/sdk-secure-storage.md](docs/sdk-secure-storage.md) shows h
 the credentials in the platform's keychain and how to derive and change the key. Groups
 are not offered to apps in the first release, so the SDK has no group calls yet.
 
+From Node.js, `bindings/node` (napi-rs) gives the same client with promises, one event
+listener and the secret store as a JavaScript object (`cd bindings/node; npm test`). From
+C, `bindings/c` builds a shared library with a generated header,
+`bindings/c/include/push_sdk.h`: opaque handles, error codes, UTF-8 JSON and blocking calls
+(`cargo test -p push-sdk-c` compiles and runs a C program against it). Both report the
+same error codes as Rust: [docs/sdk-bindings.md](docs/sdk-bindings.md) is the guide.
+
 ## Run it on minikube
 
 The whole system runs on a local [minikube](https://minikube.sigs.k8s.io/) cluster with
```

Change `bindings/ffi/src/json.rs` (save the block to a file and `git apply` it):

```diff
diff --git a/bindings/ffi/src/json.rs b/bindings/ffi/src/json.rs
index 2d396aa..0e6fb60 100644
--- a/bindings/ffi/src/json.rs
+++ b/bindings/ffi/src/json.rs
@@ -385,4 +385,92 @@ mod tests {
         assert_eq!(platform("toaster").unwrap_err().code(), "invalid");
         assert!(id::<UserId>("not an id", "userId").is_err());
     }
+
+    /// docs/sdk-bindings.md tells app developers every code the SDK makes, whether it is
+    /// retryable, and every event.
+    #[test]
+    fn the_bindings_guide_names_every_error_code_and_event() {
+        let guide = include_str!("../../../docs/sdk-bindings.md");
+        let text = String::new;
+        let errors = [
+            SdkError::Invalid(text()),
+            SdkError::NotSignedIn,
+            SdkError::DeviceRemoved,
+            SdkError::OpenedElsewhere,
+            SdkError::Offline,
+            SdkError::Disconnected,
+            SdkError::Timeout,
+            SdkError::UnexpectedAnswer(text()),
+            SdkError::ConnectionBusy,
+            SdkError::Network(text()),
+            SdkError::Storage(text()),
+            SdkError::WrongKey,
+            SdkError::EncryptionUnavailable,
+            SdkError::Secrets(text()),
+            SdkError::Closed,
+        ];
+        for failure in &errors {
+            let retryable = if failure.is_retryable() { "yes" } else { "no" };
+            let row = format!("| `{}` | {retryable} |", failure.code());
+            assert!(guide.contains(&row), "{row}");
+        }
+        assert!(guide.contains("| `panic` | no |"));
+        let (conversation_id, user) = (ConversationId::new(), UserId::new());
+        let message = Message {
+            conversation_id,
+            client_msg_id: ClientMsgId::new(),
+            message_id: None,
+            seq: None,
+            sender_id: user,
+            kind: MessageKind::Text,
+            body: None,
+            client_ts: None,
+            server_ts: None,
+            edited_at: None,
+            deleted: false,
+            status: MessageStatus::Pending,
+        };
+        let events = [
+            Event::ConnectionStateChanged(ConnectionState::Offline),
+            Event::MessageAdded(message.clone()),
+            Event::MessageSent(message.clone()),
+            Event::MessageEdited(message.clone()),
+            Event::MessageDeleted(message),
+            Event::SendFailed {
+                conversation_id,
+                client_msg_id: ClientMsgId::new(),
+                kind: OutboxKind::Send,
+                error: SdkError::Offline,
+            },
+            Event::ReceiptChanged {
+                conversation_id,
+                peer_delivered_seq: 0,
+                peer_read_seq: 0,
+            },
+            Event::ConversationChanged(Conversation {
+                id: conversation_id,
+                kind: ConversationKind::Direct,
+                peer: Some(user),
+                title: None,
+                last_seq: 0,
+                read_seq: 0,
+                unread: 0,
+                peer_delivered_seq: 0,
+                peer_read_seq: 0,
+                updated_at: 0,
+            }),
+            Event::ContactChanged(Contact {
+                user_id: user,
+                state: ContactState::Accepted,
+            }),
+            Event::SignedOut,
+            Event::DeviceRemoved,
+            Event::OpenedElsewhere,
+            Event::EventsLost,
+        ];
+        for shown in events.iter().map(event) {
+            let row = format!("| `{}` |", shown["type"].as_str().unwrap());
+            assert!(guide.contains(&row), "{row}");
+        }
+    }
 }
```

Create `docs/sdk-bindings.md` (save the block to a file and `git apply` it):

````diff
diff --git a/docs/sdk-bindings.md b/docs/sdk-bindings.md
new file mode 100644
index 0000000..275d6a9
--- /dev/null
+++ b/docs/sdk-bindings.md
@@ -0,0 +1,279 @@
+# The SDK from Node.js and C
+
+`push-sdk` is written in Rust. Two bindings let apps use it from other languages:
+
+- **`bindings/node`** — a Node.js package (napi-rs). Every call that talks to a server
+  or touches the database returns a promise; events come to a listener; the app's secret
+  store is a JavaScript object.
+- **`bindings/c`** — a shared library with a C header (`include/push_sdk.h`, generated by
+  cbindgen and checked in). Handles are opaque, errors are codes plus messages, data comes
+  as UTF-8 JSON, and calls that talk to a server block the calling thread.
+
+Both only convert and wait: the connection, the outbox, the local store, retries and
+reconnects are the Rust core's (spec §8.1). Both hand apps the same data, shaped once in
+`bindings/ffi` (`push-sdk-ffi`), and the same error codes.
+
+## What every binding shares
+
+### Error codes
+
+Every failure has a stable `code`, identical in Rust (`SdkError::code`), Node
+(`PushError.code`) and C (`push_error_code`), and says whether the same call may succeed
+later (`retryable`). A refusal by a server keeps the server's own code (`not_contact`,
+`invite_not_found`, `invalid_credentials`, `rate_limited`, …); a rate limit also gives
+`retryAfterS`. The SDK's own codes:
+
+| Code | Retryable | Meaning |
+|---|---|---|
+| `invalid` | no | The input breaks a rule (an empty text, an id that is not a UUID, an unknown conversation, a 31-byte key); nothing was stored or sent. Also a call a binding refuses (from an SDK thread, a second event listener). |
+| `not_signed_in` | no | No device is signed in: sign in again. |
+| `device_removed` | no | The server revoked this device (close code 4003). |
+| `opened_elsewhere` | no | Another copy of the app's data took over the connection (4007). |
+| `offline` | yes | No connection came up within the request timeout, or the app has not asked to connect. |
+| `disconnected` | yes | The connection dropped before the answer; the request may or may not have been carried out. |
+| `timeout` | no | The request was sent and no answer came in time: it may have been carried out, so check before repeating it. |
+| `unexpected_answer` | no | The server answered something this client does not understand. |
+| `connection_busy` | yes | A sign-in or sign-out could not stop the connection in time; nothing was changed. |
+| `network` | yes | A server could not be reached. |
+| `storage` | no | The local database failed. |
+| `wrong_key` | no | The database key is wrong or missing; nothing was changed. |
+| `encryption_unavailable` | no | A key was given, but the linked SQLite cannot encrypt. Do not retry. |
+| `secrets` | no | The app's secret store failed (or, in Node, did not answer within 30 s). |
+| `closed` | no | The client was closed. |
+| `panic` | no | A bug in the SDK, caught at the boundary (C and Node only). Close the client. |
+
+### Data
+
+Data is JSON-shaped: field names and enum values are camelCase, ids are UUID strings, times
+are microseconds since the Unix epoch (they fit JavaScript numbers exactly), and absent
+values are `null`.
+
+- **Account:** `{userId, deviceId}`.
+- **Connection state:** `{state}` — `offline`, `connecting`, `connected`, `signedOut`,
+  `deviceRemoved`, `openedElsewhere`, or `reconnecting` with `attempt` and `retryInMs`.
+- **Message:** `{conversationId, clientMsgId, messageId, seq, senderId, kind, body,
+  clientTs, serverTs, editedAt, deleted, status}`; `kind` is `text` or `system`; `status`
+  is `pending`, `sent`, `delivered`, `read` (the user's own) or `received` (others');
+  `body` is `null` once deleted. `clientMsgId` is the handle for edit, delete and cancel.
+- **Conversation:** `{id, kind, peer, title, lastSeq, readSeq, unread, peerDeliveredSeq,
+  peerReadSeq, updatedAt}`; `kind` is `direct`, `group` or `unknown`.
+- **Contact:** `{userId, state}`; `state` is `none`, `requestedOut`, `requestedIn`,
+  `accepted` or `blocked`.
+- **Invite:** `{code, expiresAt, maxUses, uses}`. **Profile:** `{userId, handle,
+  displayName}`. **Device:** `{deviceId, name, platform, createdAt, lastSeenAt, current}`.
+- **Error** (inside `sendFailed`): `{code, message, retryable, retryAfterS}`.
+
+### Events
+
+One ordered stream per client, each event an object with a `type`:
+
+| `type` | Fields |
+|---|---|
+| `connectionStateChanged` | the connection state's fields |
+| `messageAdded` | `message`: the local echo of a send, or a message received |
+| `messageSent` | `message`: stored by the server, with its `seq` |
+| `messageEdited` | `message` |
+| `messageDeleted` | `message`, its `body` gone |
+| `sendFailed` | `conversationId`, `clientMsgId`, `kind` (`send`, `edit`, `delete`), `error` |
+| `receiptChanged` | `conversationId`, `peerDeliveredSeq`, `peerReadSeq` |
+| `conversationChanged` | `conversation` |
+| `contactChanged` | `contact` |
+| `signedOut` | sign in again |
+| `deviceRemoved` | the device was revoked |
+| `openedElsewhere` | another copy took over |
+| `eventsLost` | the app stopped reading and 10,000 events waited: reload what the UI shows from the local data |
+
+### The secret store and the database key
+
+The credentials live only in a store the app provides, under the keys `push.session`,
+`push.device` and `push.registration`. [sdk-secure-storage.md](sdk-secure-storage.md)
+explains the rules and shows the platforms' keychains; the bindings take the same store as
+callbacks. The SDK ships no keychain store (owner decision, Plan 9).
+
+The optional database key is 32 bytes, given apart from the other settings as bytes, so it
+never sits in a string or a log. Derive it from a PIN or passphrase with the bindings'
+`deriveDatabaseKey` / `push_derive_database_key` and a salt the app keeps next to the
+database (`newSalt` / `push_new_salt`); the SDK never stores it.
+
+## Node.js
+
+### Build and test
+
+Node 18 or later, and the Rust toolchain:
+
+    cd bindings/node
+    npm run build      # cargo build -p push-sdk-node, then copies it to push_sdk.node
+    npm test           # builds the addon and push-sdk-fakes, then node --test
+    npm ci; npm run typecheck   # tsc checks index.d.ts against test/types.ts
+
+`npm run build -- --release` builds it optimised. The package is `index.js`, `index.d.ts`
+and `push_sdk.node` (built for one platform); it is not published yet.
+
+### Use
+
+```js
+const { Client, PushError, deriveDatabaseKey, newSalt } = require('push-sdk');
+
+const client = await Client.open(
+  { authUrl: 'https://push.example', socketUrl: 'wss://push.example/v1/socket', databasePath: 'push.db' },
+  secretStore, // { load(key), save(key, value), delete(key) }, each may return a promise
+);
+client.onEvent((event) => {
+  if (event.type === 'messageAdded') render(event.message);
+});
+if (!client.account()) await client.signIn(email, password, 'Alice laptop', 'desktop');
+client.connect();
+const chat = await client.openDirect(bobUserId);
+await client.send(chat.id, 'hello');
+try {
+  await client.redeemInvite(code);
+} catch (error) {
+  if (error instanceof PushError && error.code === 'invite_not_found') showInviteGone();
+}
+await client.close();
+```
+
+- `Client.open` loads the credentials from the store, then opens the database on a thread
+  of its own (a key is stretched, the schema migrated): the JavaScript thread never waits.
+- `account()`, `connectionState()`, `connect()`, `disconnect()` and `onEvent()` answer at
+  once; everything else returns a promise.
+- One listener per client, called on the JavaScript thread, in order. While it is set,
+  Node keeps running, as with an open socket: call `close()` when done. An exception
+  thrown by the listener is reported as an uncaught exception.
+- A call made after `close()` fails with `closed`; a call still running when the client
+  closes rejects with `closed` rather than staying pending.
+
+### The secret store from JavaScript
+
+The SDK calls its store with blocking calls on its own threads, while a JavaScript store
+answers on the JavaScript thread, usually with promises. A blocking call into JavaScript
+from a thread the JavaScript thread waits for would deadlock, so the binding keeps two
+rules:
+
+- **Reads never reach JavaScript after opening.** `Client.open` loads the three values once
+  (while the JavaScript thread is free) and keeps them in memory; every later read comes
+  from there, so the calls that answer at once never wait for JavaScript.
+- **Writes wait for JavaScript, never on the JavaScript thread.** `save` and `delete` go
+  to the store through a threadsafe function, and the SDK thread waits (up to 30 s) for the
+  answer; the copy in memory changes only once the store confirmed the write, so the
+  credentials are still saved before they are used. Every call that writes runs on an SDK
+  thread and returns a promise, so the JavaScript thread is free to answer.
+
+So: never change the store behind an open client's back, and never call the client from
+inside the store's methods (a store that awaits the client's own sign-out would wait for
+itself until the 30 s are up).
+
+A store on the platform's keychain, with the
+[`@napi-rs/keyring`](https://www.npmjs.com/package/@napi-rs/keyring) package (Windows
+Credential Manager, macOS Keychain, Linux Secret Service); the values are bytes, kept as
+base64:
+
+```js
+const { Entry } = require('@napi-rs/keyring');
+
+const entry = (key) => new Entry('com.example.chat', key);
+const keychainStore = {
+  load(key) {
+    const text = entry(key).getPassword();
+    return text == null ? null : Buffer.from(text, 'base64');
+  },
+  save(key, value) {
+    entry(key).setPassword(Buffer.from(value).toString('base64'));
+  },
+  delete(key) {
+    entry(key).deletePassword();
+  },
+};
+```
+
+### Errors
+
+Every failure is a `PushError` with `code`, `message`, `retryable` and `retryAfterS`. An
+argument of the wrong type is `invalid`. Messages never contain a token, a key or a
+message's text.
+
+## C
+
+### Build and link
+
+    cargo build --release -p push-sdk-c
+
+gives `push_sdk_c.dll` (with the import library `push_sdk_c.dll.lib`) on Windows,
+`libpush_sdk_c.so` on Linux and `libpush_sdk_c.dylib` on macOS, in `target/release`.
+Include `bindings/c/include/push_sdk.h` and link the library:
+
+    cl /I bindings\c\include app.c /link target\release\push_sdk_c.dll.lib
+    cc -I bindings/c/include app.c -L target/release -lpush_sdk_c -o app
+
+The header is generated by cbindgen from the Rust code. `cargo test -p push-sdk-c` fails
+when it is stale (CI runs it); regenerate it with
+`PUSH_SDK_BLESS_HEADER=1 cargo test -p push-sdk-c --test header`.
+
+### The rules
+
+- **Handles.** `push_client_open` creates a `PushClient`; `push_client_destroy` stops the
+  connection, closes the database and frees it (it waits up to a few seconds). No call may
+  use the client during or after `push_client_destroy`.
+- **Errors.** A function that can fail returns `PUSH_OK` (0) or `PUSH_ERROR` (-1). Given a
+  non-NULL `PushError **error`, it stores an error the caller reads with
+  `push_error_code`, `push_error_message`, `push_error_retryable` and
+  `push_error_retry_after_s` and frees with `push_error_free`. The strings they return
+  live as long as the error.
+- **Strings.** Arguments are UTF-8, NUL-terminated, borrowed for the call. Results
+  (`char **`) are UTF-8 — JSON for anything with fields — and belong to the caller, who
+  frees them with `push_string_free`. JSON keeps the ABI stable as the SDK grows: a new
+  field is not a new struct layout. Every output pointer is checked before the call does
+  anything, so a call never acts and then fails to report.
+- **Blocking calls.** Calls that talk to a server (`push_client_sign_in`,
+  `push_client_open_direct`, …) block the calling thread until they end. Each has the SDK's
+  own limits (a request waits 15 s for the connection and 15 s for the answer), so none
+  blocks for ever; call them from the app's worker threads, not a UI thread. Blocking was
+  chosen over completion callbacks: a C app has its own threads already, the results stay
+  on the caller's stack with plain ownership, and no context pointer has to outlive a call
+  on an SDK thread. A blocking call made on one of the SDK's threads is refused with
+  `invalid` instead of waiting there.
+- **Panics** never cross the boundary: they become the code `panic`.
+
+### Events
+
+Either give a callback, once:
+
+```c
+static void on_event(void *context, const char *event_json) { /* parse, post to the UI */ }
+push_client_set_event_callback(client, on_event, app, release_app, &error);
+```
+
+It is called on an SDK thread of its own (`push-sdk-events`), one event at a time and in
+order; the JSON is borrowed for the call. It may call the SDK, blocking calls included.
+After the client is destroyed and the last event delivered, `release(context)` is called
+once. Or poll: `push_client_next_event(client, timeout_ms, &event_json, &error)` stores the
+next event, or NULL when none came in time. A client uses one or the other.
+
+### The secret store
+
+```c
+PushSecretStore store = {
+    .context = keychain,
+    .load = keychain_load,       /* PUSH_OK with a value the app allocated, PUSH_NOT_FOUND, or PUSH_ERROR */
+    .save = keychain_save,
+    .remove = keychain_remove,
+    .free_value = keychain_free, /* the SDK hands back what load allocated */
+    .release = keychain_release, /* once, when the client no longer needs the store */
+};
+push_client_open(config_json, key, key ? PUSH_DATABASE_KEY_LEN : 0, store, &client, &error);
+```
+
+The functions are called on SDK threads and on the threads that call the SDK, so they
+must be thread-safe; a call into the SDK from inside them fails with `invalid`. The store
+is the client's from `push_client_open` on and is released even when the open fails.
+`bindings/c/tests/c/flow.c` is a complete program: an in-memory store, sign-in, events by
+polling and by callback, sending, and an encrypted database.
+
+## Testing the bindings
+
+`push-sdk`'s `testing` feature exposes its fake auth-svc and a gateway that answers by
+itself (every send is stored and answered by the peer with the same text);
+`push-sdk-fakes` runs both and prints their URLs. `cargo test -p push-sdk-c` compiles
+`tests/c/flow.c` with the platform's C compiler (MSVC or cc) and runs it against them;
+`npm test` in `bindings/node` runs the Node tests against the binary. The CI's `bindings`
+job runs both on Linux.
````

Change `docs/sdk-secure-storage.md` (save the block to a file and `git apply` it):

```diff
diff --git a/docs/sdk-secure-storage.md b/docs/sdk-secure-storage.md
index 4fdbc65..14f83af 100644
--- a/docs/sdk-secure-storage.md
+++ b/docs/sdk-secure-storage.md
@@ -14,7 +14,8 @@ protected:
 The SDK ships no keychain integration (owner decision): every platform has its own, and
 the app already knows which one it runs on. This guide shows how to connect the SDK to the
 platform's secure storage, what never to do, and how to derive and change the database
-key.
+key. Apps on the Node.js and C bindings give the same store as callbacks:
+[sdk-bindings.md](sdk-bindings.md) shows how, with a keychain store for Node.js.
 
 ## The secret store
 
```

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test -p push-sdk-ffi --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A .github README.md docs bindings
git commit -m "docs: the SDK from Node.js and C; ci: the bindings job"
```

Expected: `the_bindings_guide_names_every_error_code_and_event` passes; 978 tests
pass, no warnings, no diff.

---

### Task 8: The Node binding on minikube

**Files:**
- Create: `bindings/node/examples/minikube.js`, `deploy/minikube/node-example.ps1`
- Modify: `.dockerignore`, `README.md`

- [ ] **Step 1: Write the code**

Change `.dockerignore` (save the block to a file and `git apply` it):

```diff
diff --git a/.dockerignore b/.dockerignore
index 44e53f9..bd3e20b 100644
--- a/.dockerignore
+++ b/.dockerignore
@@ -6,3 +6,6 @@ deploy
 docs
 target
 **/*.md
+# The Node binding's local install and build output.
+**/node_modules
+**/*.node
```

Change `README.md` (save the block to a file and `git apply` it):

```diff
diff --git a/README.md b/README.md
index fd114c7..5093dd3 100644
--- a/README.md
+++ b/README.md
@@ -85,6 +85,11 @@ Secret, the PostgreSQL volume and its data directory (`-Delete` deletes the prof
 so a later install reuses both and the passwords keep matching. Every pull request is
 verified this way before it is merged.
 
+With the cluster up, `deploy/minikube/node-example.ps1` runs the Node.js binding the same
+way: it forwards local ports to the ingress and to the DEV ONLY mailbox, and two Node.js
+apps (`bindings/node/examples/minikube.js`) sign up, verify, sign in, become contacts by
+invite and chat with a read receipt.
+
 The minikube values switch on a DEV ONLY shortcut: the worker, which logs emails instead
 of sending them, shows them at `/dev/mailbox`, which the ingress does not route (it
 reaches the worker only at `/v1/webhooks/resend`). Clients, the smoke test included,
```

Create `bindings/node/examples/minikube.js`:

```js
'use strict';

// Two Node.js apps on push-sdk against a running deployment (the minikube cluster, through
// its ingress): they sign up, verify their email from the DEV ONLY mailbox as a browser
// would open the link, sign in, become contacts by invite and chat with a read receipt.
// deploy/minikube/node-example.ps1 forwards the ports and runs it:
//
//   node examples/minikube.js --auth http://127.0.0.1:18080 --socket ws://127.0.0.1:18080/v1/socket \
//     --host push.local --mailbox http://127.0.0.1:18081
//
// It prints ids, seqs, codes and timings, never a token, a password or a message's text.

const http = require('node:http');
const { randomUUID } = require('node:crypto');

const { Client, PushError } = require('..');

const args = Object.fromEntries(
  process.argv.slice(2).reduce((pairs, value, index, all) => (index % 2 === 0 ? [...pairs, [value.replace(/^--/, ''), all[index + 1]]] : pairs), []),
);
for (const name of ['auth', 'socket', 'mailbox']) {
  if (!args[name]) {
    console.error('usage: node examples/minikube.js --auth <url> --socket <ws-url> --mailbox <url> [--host <host>]');
    process.exit(2);
  }
}

const started = Date.now();
const step = (text) => console.log(`node example: ${text}`);

/** A plain HTTP request with the ingress's virtual host; resolves to [status, JSON body]. */
function request(method, url, body, host) {
  const target = new URL(url);
  const payload = body == null ? null : JSON.stringify(body);
  return new Promise((resolve, reject) => {
    const outgoing = http.request(
      target,
      {
        method,
        headers: {
          ...(host ? { host } : {}),
          ...(payload ? { 'content-type': 'application/json', 'content-length': Buffer.byteLength(payload) } : {}),
        },
      },
      (response) => {
        let text = '';
        response.setEncoding('utf8');
        response.on('data', (chunk) => (text += chunk));
        response.on('end', () => resolve([response.statusCode, text ? JSON.parse(text) : null]));
      },
    );
    outgoing.on('error', reject);
    if (payload) outgoing.write(payload);
    outgoing.end();
  });
}

/** Opens the verification link the worker delivered to the DEV ONLY mailbox. */
async function verify(email) {
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    const [, mailbox] = await request('GET', `${args.mailbox}/dev/mailbox?to=${encodeURIComponent(email)}`);
    const found = (mailbox.emails || []).filter((sent) => sent.template === 'verify_email').pop();
    if (found) {
      const link = found.text.split('\n').find((line) => line.startsWith('http'));
      const token = new URL(link).searchParams.get('token');
      const [status] = await request('POST', `${args.auth}/v1/auth/verify`, { token }, args.host);
      if (status !== 200) throw new Error(`verify answered ${status}`);
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`no verification email for ${email} within 30 s`);
}

/** A store in memory: an app would use the platform's keychain (docs/sdk-bindings.md). */
function memoryStore() {
  const values = new Map();
  return {
    load: async (key) => values.get(key) ?? null,
    save: async (key, value) => {
      values.set(key, Buffer.from(value));
    },
    delete: async (key) => {
      values.delete(key);
    },
  };
}

/** An app with its own client: signed up, verified, signed in and connected. */
async function app(name) {
  const suffix = randomUUID().replace(/-/g, '').slice(-10);
  const email = `${name}.${suffix}@node.example`;
  const password = `node-${randomUUID()}`;
  const client = await Client.open(
    { authUrl: args.auth, socketUrl: args.socket, host: args.host, databasePath: ':memory:' },
    memoryStore(),
  );
  const events = [];
  const waiting = [];
  client.onEvent((event) => {
    events.push(event);
    for (const waiter of [...waiting]) {
      if (waiter.match(event)) {
        waiting.splice(waiting.indexOf(waiter), 1);
        waiter.resolve(event);
      }
    }
  });
  const next = (match) =>
    events.find(match) ||
    new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`${name}: no such event within 15 s`)), 15000);
      waiting.push({ match, resolve: (event) => (clearTimeout(timer), resolve(event)) });
    });
  const begun = Date.now();
  await client.signUp(email, password, `${name}_${suffix}`, name);
  await verify(email);
  const account = await client.signIn(email, password, `${name}'s laptop`, 'desktop');
  client.connect();
  await client.waitConnected(30000);
  const reconnects = events.filter((event) => event.type === 'connectionStateChanged' && event.state === 'reconnecting').length;
  step(
    `${name} signed up, verified, signed in and connected (device ${account.deviceId}, ${reconnects} reconnects while the device was not relayed yet) in ${Date.now() - begun} ms`,
  );
  return { name, client, account, next };
}

async function main() {
  const ann = await app('ann');
  const ben = await app('ben');

  const invite = await ann.client.createInvite(3600, 1);
  const inviter = await ben.client.redeemInvite(invite.code);
  if (inviter !== ann.account.userId) throw new Error('the invite names someone else');
  await ann.next((event) => event.type === 'contactChanged' && event.contact.userId === ben.account.userId && event.contact.state === 'accepted');
  step("ben redeemed ann's invite; ann's app heard the new contact");

  const conversation = await ben.client.openDirect(ann.account.userId);
  const sentAt = Date.now();
  const echo = await ben.client.send(conversation.id, 'hello from Node on minikube');
  const sent = await ben.next((event) => event.type === 'messageSent' && event.message.clientMsgId === echo.clientMsgId);
  const received = await ann.next((event) => event.type === 'messageAdded' && event.message.clientMsgId === echo.clientMsgId);
  step(`ben's message was stored (seq ${sent.message.seq}) and ann's app received it (${received.message.body.length} characters) after ${Date.now() - sentAt} ms`);

  await ann.client.markRead(conversation.id, received.message.seq);
  const receipt = await ben.next((event) => event.type === 'receiptChanged' && event.peerReadSeq >= received.message.seq);
  step(`ann read it; ben's app heard the receipt (read up to seq ${receipt.peerReadSeq})`);

  try {
    await ann.client.redeemInvite('AAAAAAAAAA');
    throw new Error('an unknown invite was redeemed');
  } catch (error) {
    if (!(error instanceof PushError) || error.code !== 'invite_not_found') throw error;
    step(`an unknown invite failed with ${error.code} (retryable: ${error.retryable})`);
  }

  await Promise.all([ann.client.close(), ben.client.close()]);
  step(`PASS in ${Date.now() - started} ms`);
}

main().catch((error) => {
  console.error(`node example: FAIL: ${error instanceof PushError ? `${error.code}: ${error.message}` : error.message}`);
  process.exit(1);
});
```

Create `deploy/minikube/node-example.ps1`:

```powershell
<#
.SYNOPSIS
Runs the Node.js SDK example against push-messenger on minikube.

.DESCRIPTION
Builds the Node binding, forwards a local port to the cluster's ingress controller and one
to the worker's DEV ONLY mailbox, and runs bindings/node/examples/minikube.js: two apps sign
up, verify, sign in, become contacts by invite and chat with a read receipt, through the
ingress like any client. The cluster must be up (deploy/minikube/up.ps1). The port
forwards are stopped at the end.

.EXAMPLE
deploy/minikube/node-example.ps1
#>
[CmdletBinding()]
param(
    # The minikube profile; also the kubectl context.
    [string] $Cluster = 'push',
    [int] $IngressPort = 18080,
    [int] $MailboxPort = 18081
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$node = Join-Path $root 'bindings\node'

# Waits until something listens on a local port.
function Wait-Port([int] $Port) {
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        $client = [Net.Sockets.TcpClient]::new()
        try {
            $client.Connect('127.0.0.1', $Port)
            return
        } catch {
            Start-Sleep -Milliseconds 500
        } finally {
            $client.Dispose()
        }
    }
    throw "nothing listens on port $Port"
}

Write-Host '==> build the Node binding' -ForegroundColor Cyan
node (Join-Path $node 'scripts\build.js')
if ($LASTEXITCODE -ne 0) {
    throw 'the build failed'
}

$forwards = @(
    Start-Process kubectl -PassThru -WindowStyle Hidden -ArgumentList @(
        '--context', $Cluster, '-n', 'ingress-nginx', 'port-forward',
        'svc/ingress-nginx-controller', "${IngressPort}:80")
    Start-Process kubectl -PassThru -WindowStyle Hidden -ArgumentList @(
        '--context', $Cluster, '-n', 'push', 'port-forward', 'svc/push-worker', "${MailboxPort}:8080")
)
try {
    Wait-Port $IngressPort
    Wait-Port $MailboxPort
    Write-Host '==> run the Node.js example' -ForegroundColor Cyan
    node (Join-Path $node 'examples\minikube.js') `
        --auth "http://127.0.0.1:$IngressPort" `
        --socket "ws://127.0.0.1:$IngressPort/v1/socket" `
        --host push.local `
        --mailbox "http://127.0.0.1:$MailboxPort"
    if ($LASTEXITCODE -ne 0) {
        throw 'the Node.js example failed'
    }
} finally {
    foreach ($forward in $forwards) {
        Stop-Process -Id $forward.Id -ErrorAction SilentlyContinue
    }
}
```

Commit it (the gates of Task 7 still hold: nothing here is Rust):

```powershell
cargo fmt --all -- --check
git add -A .dockerignore README.md bindings deploy
git commit -m "push-sdk-node: an example on minikube, through the ingress"
```

- [ ] **Step 2: Note the kubectl context**

```powershell
kubectl config current-context
```

Expected: `minikube` (restore it in Step 6).

- [ ] **Step 3: Bring the cluster up and run the smoke test**

```powershell
deploy/minikube/up.ps1 -Smoke
```

Expected (verified 2026-10-15 as an upgrade of the stopped `push` profile, revision 23;
ids and timings differ): `==> start minikube profile 'push'`, the five images build (the
workspace now has the bindings' crates; the images build only the services), the
release upgrades, and `helm test` ends `Phase: Succeeded` with the smoke test of Plan 9B
unchanged, ending:

```
smoke: sdk: a send cut off by a restart was stored once (its first attempt was acknowledged before the app stopped; seq 7, the next took 8)
smoke: sdk: a copy of dana's app with her credentials connected; her first app reported OpenedElsewhere (4007) after 69 ms and stopped
smoke: sdk: the copy revoked dana's device; it reported DeviceRemoved (4003) after 165 ms and stopped
smoke: sdk: erin reset her password; her app reported SignedOut (4001) after 2021 ms, signed the same device in again and kept its 6 messages
smoke: PASS in 15066 ms
```

- [ ] **Step 4: Run the Node example**

```powershell
deploy/minikube/node-example.ps1
```

Expected (verified 2026-10-15; ids and timings differ):

```
==> build the Node binding
==> run the Node.js example
node example: ann signed up, verified, signed in and connected (device 01a13f9c-6548-71c5-a43b-3f885bdef74d, 2 reconnects while the device was not relayed yet) in 1488 ms
node example: ben signed up, verified, signed in and connected (device 01a13f9c-6d16-74e3-b6fc-560a3f30776f, 2 reconnects while the device was not relayed yet) in 2301 ms
node example: ben redeemed ann's invite; ann's app heard the new contact
node example: ben's message was stored (seq 1) and ann's app received it (27 characters) after 7 ms
node example: ann read it; ben's app heard the receipt (read up to seq 1)
node example: an unknown invite failed with invite_not_found (retryable: false)
node example: PASS in 3953 ms
```

Then check that no token reached a log:

```powershell
$gateway = kubectl --context push -n push logs deploy/push-gateway --tail=800
"gateway token-like: $(($gateway | Select-String -SimpleMatch 'eyJ').Count), bearer: $(($gateway | Select-String -SimpleMatch 'Bearer').Count)"
$auth = kubectl --context push -n push logs deploy/push-auth --tail=800
"auth token-like: $(($auth | Select-String -SimpleMatch 'eyJ').Count)"
```

Expected: `gateway token-like: 0, bearer: 0` and `auth token-like: 0`.

- [ ] **Step 5: Red — the example notices a wrong answer**

In `bindings/node/examples/minikube.js`, replace `if (inviter !== ann.account.userId)` with
`if (inviter === ann.account.userId)` and run `deploy/minikube/node-example.ps1` again.
Expected: `node example: FAIL: the invite names someone else` and the script stops with
"the Node.js example failed". Restore the line. (Each run signs up two users; the per-IP
signup limit is 20 an hour, shared with the smoke test's five.)

- [ ] **Step 6: Stop the cluster and restore the context**

```powershell
minikube stop -p push
kubectl config use-context minikube
kubectl config current-context
```

Expected: `1 node stopped.` and `minikube`. Never delete the profile; never touch other
Docker containers on this machine.

---

### Task 9: Roadmap and pull request

**Files:**
- Modify: `docs/superpowers/plans/2026-10-11-roadmap.md`

- [ ] **Step 1: Mark the roadmap**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, set the last column of the 10 row to
`In review`.

- [ ] **Step 2: Final gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add -A docs
git commit -m "docs: mark plan 10 in review"
```

Expected: 978 tests pass, no warnings, no diff.

- [ ] **Step 3: Push, open the pull request and wait for CI**

```powershell
git push -u origin feature/plan-10-sdk-bindings
$body = @'
Implements docs/superpowers/plans/2026-10-15-plan-10-sdk-bindings.md: push-sdk from Node.js and C. bindings/node (napi-rs): promises settled from the client's own runtime, one event listener through a threadsafe function, the secret store as a JavaScript object bridged without deadlocks (loaded once, written through and confirmed), PushError with the SDK's stable codes, index.d.ts checked by tsc, node:test against the SDK's fakes. bindings/c (cbindgen): opaque handles, PUSH_OK/PUSH_ERROR with error objects carrying the same codes, UTF-8 strings in (borrowed) and out (owned, push_string_free), JSON for compound data, blocking calls bounded by the SDK's limits, events by callback or polling, the secret store as function pointers, the header checked in and tested for staleness, a C program run with MSVC and gcc. bindings/ffi shapes the data and error codes for both. push-sdk gains Client::spawn, DatabaseKey::as_bytes and its test doubles behind a testing feature. A CI job runs the bindings on Linux. Verified on minikube: up.ps1 -Smoke passed, and deploy/minikube/node-example.ps1 ran two Node apps through the ingress (sign-up, invite, chat, read receipt).
'@
gh pr create --base main --title "Plan 10: the SDK bindings (Node.js and C)" --body $body
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust`, `msrv`, `bindings`, `images`, `chart`). Do not
merge; the controller merges after the final review.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §8.1 the bindings only convert types and adapt async calls and callbacks | 2, 3, 4, 5, 6 |
| §8.5 every `extern "C"` function wrapped in `catch_unwind` | 3, 4 |
| §8.5 the core owns exactly one tokio runtime (`Client::spawn`, `block_on`) | 1, 2, 3, 5 |
| §8.5 C events through a registered callback and context pointer, on an SDK thread | 3 |
| §8.5 every returned allocation has a matching `*_free` | 3, 4 |
| §8.5 Node receives events through a `ThreadsafeFunction` | 5 |
| §8.2 the whole public surface, no group calls | 3, 4, 5, 6 |
| §8.2 `SecretStore` by the app (callbacks), no platform store (owner decision) | 3, 5, 7 |
| §8.2 `Config::database_key` as bytes, never logged | 2, 3, 5 |
| Stable error codes identical in Rust, Node and C | 2, 3, 4, 5, 6, 7 |
| §10 `bindings/node`, `bindings/c` | 3, 5 |
| §12 the bindings tested against the fakes and, for Node, the whole stack on minikube | 4, 6, 8 |

## Verified in a worktree

Every task was applied in order to a worktree of `main` at `813bbfc` and committed; each
commit passed the gates. `cargo test --workspace --locked` counts after each task: 966
(baseline), 968, 972, 976, 977, 977, 977, 978, 978; clippy, fmt and the 1.85 build clean
at every commit; `npm test` 6 of 6 after Task 5 and 10 of 10 after Tasks 6 to 8, `tsc`
clean from Task 6; the C program passed from Task 4 on. Every Step 2 mutation (Task 7:
its Step 1) failed exactly as recorded above. Docker-backed tests of other crates
(`push-auth`, `push-gateway`) can fail with testcontainers' `PortNotExposed` on a loaded
Windows machine — on `main` too; a rerun passes.

On Linux (`rust:1.85-bookworm`, gcc 12, Node 18.20, from a bundle of the branch):
`cargo test -p push-sdk-c -p push-sdk-ffi` passed — the header generated on Windows
matched byte for byte — and `npm ci`, `npm test` (10 of 10) and `npm run typecheck`
passed. That run found one bug, fixed in Task 4 before the plan was written: the C
program read its error pointer in the same call's argument list, and gcc evaluates
arguments in another order than MSVC.

The plan's blocks, replayed onto a fresh worktree of `main` with the header generated as
the tasks say, reproduce every verified commit's tree exactly.

## Verified on minikube

Run on 2026-10-15 (profile `push`, Kubernetes 1.31) from the verification worktree, after
Task 8: `deploy/minikube/up.ps1 -Smoke` started the stopped profile, built the five
images from the tree with the bindings in the workspace, upgraded the release (revision
23) and `helm test` ended `Phase: Succeeded`, the smoke test unchanged (`smoke: PASS in
15066 ms`). `deploy/minikube/node-example.ps1` then ran two Node apps through the
ingress: both connected after two 4006 reconnects each, became contacts by invite,
exchanged a message (stored at seq 1, received 7 ms later) and a read receipt, and saw
`invite_not_found`; `node example: PASS in 3953 ms`. No token-like string or `Bearer` in
the gateway's or auth-svc's logs. Then `minikube stop -p push` and the kubectl context back
to `minikube`.

Again from the final tree, after the gates: `up.ps1 -SkipBuild -Smoke` (revision 24,
`smoke: PASS in 15838 ms`); Task 8's red step (`node example: FAIL: the invite names
someone else`, the script failing and stopping its port forwards); the example restored
(`node example: PASS in 4056 ms`); no token in the logs; the cluster stopped and the
context restored.

## Deferred to later plans

- **Packages and prebuilt binaries** (Owner decisions needed, 3): npm name and scope,
  per-platform addon packages built in CI, the C library and header as release assets,
  `engines` raised to `>=20`.
- **Mobile** (Owner decisions needed, 2): cross-compiling the SDK and SQLite3 Multiple
  Ciphers for iOS and Android; Swift and Kotlin packages (UniFFI) or C glue.
- **Typed C structs** beside the JSON results, if the owner wants them (Owner decisions
  needed, 1).
- **Completion callbacks in C** (non-blocking variants of the server calls) and a way to
  cancel a blocking call; today each call ends within the SDK's own limits.
- **An async iterator for Node events** (`for await (const event of client.events())`)
  over `onEvent`.
- **A configurable secret-store timeout in Node** (30 s fixed) and a store that changes
  behind an open client (the bridge's copy would be stale until reopened).
- **`Client::open_async` in the core:** the bindings open on a thread of their own (Node)
  or document `push_client_open` as slow (C) instead.
- **A C example on minikube:** the C program runs against the fakes in every test run; on
  the cluster only the Node example runs.
- **The fake gateway's coverage:** `AutoGateway` answers sends, edits, deletes, invites
  and listings; receipts, contact events and history pages are tested in `push-sdk`'s own
  tests and on minikube (smoke test and Node example), not through the bindings' fakes.
- **macOS and Windows in CI:** the `bindings` job runs on Linux; Windows is verified
  locally (MSVC), macOS not at all yet.
- **napi 3.5+ and newer napi-derive** need Rust 1.88; the workspace stays on 1.85, so the
  binding is pinned to napi 3.4.0, napi-derive 3.3.0 and napi-build 2.2.4 until the MSRV
  moves.
- **A promise settled while Node shuts down:** napi's `JsDeferred` asserts in debug builds
  if its threadsafe function is already closed; it only happens when the process exits
  with calls still running.
- **The groups API** (owner decision): no group calls in either binding until groups are
  offered.
