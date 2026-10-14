# Plan 9B — SDK core: minikube Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove Plan 9A end to end, the way a third-party app uses the system: two apps built on `push-sdk` run against the whole stack through the ingress, locally (the stack test of `push-smoke`) and on the minikube cluster (the chart's `helm test`). Dana and Erin sign up and in with the SDK (it reconnects by itself while the gateway answers 4006), become contacts by invite, chat with a delivery and a read receipt, edit and delete; Erin queues messages offline and restarts her app, and the outbox sends them after the restart, in order and once; a send cut off by a restart is retried with its `client_msg_id` and stored once; Dana revokes her device and the SDK reports `DeviceRemoved` (4003) and stops; Erin resets her password and the SDK reports `SignedOut` (4001), and she signs the same device in again with her history kept. The raw-protocol smoke test of Plan 8B stays in front of it. `test-chart.ps1` has 32 checks.

**Architecture:** `push-smoke` gains `sdk.rs`, the SDK scenario, run after the raw one by the same binary, in the same helm test Pod; it depends on `push-sdk` and reuses the smoke test's auth helper only for what a user does in a browser (opening the verification and reset links from the DEV ONLY mailbox). Each app has its own SQLite file in a directory the smoke test is given (`--data-dir`, `PUSH_SMOKE_DATA_DIR`): the stack test uses Cargo's target temp directory; the smoke Pod mounts an `emptyDir` at `/data`, its root filesystem staying read-only. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §8, §11, §12; roadmap "Running locally on minikube".

**Tech Stack:** Docker (BuildKit), minikube 1.34 (Kubernetes 1.31, docker driver, ingress addon), kubectl, Helm 3.16, PowerShell 7 and bash. The builder image (`rust:1.85-slim-bookworm`) compiles SQLite for `rusqlite` with its gcc; the runtime images stay distroless.

---

## Before you start

- **Plan 9A is merged.** This plan changes the smoke test, the smoke Pod and the README.
- **The spec is already revised** on `main` for Plan 9 (committed with the plans), so no
  task changes it; Task 3 only marks the roadmap.
- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Create
  the branch from an up-to-date `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-09b-sdk-minikube
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
$env:CARGO_INCREMENTAL = '0'
```

- **Docker Desktop must be running**, with the resources of Plan 4B. The minikube profile
  is `push` (Kubernetes 1.31). Note the kubectl context first and restore it at the end —
  `minikube stop` clears it:

```powershell
kubectl config current-context
```

- **No wipe is needed**: no database, Secret or script changes. If `up` stops with "run
  down.ps1 first" anyway, run `deploy/minikube/down.ps1` once. Never delete the profile,
  and never touch other Docker containers on this machine.
- Helm may print `failed to load plugins … helm-diff … unknown field "platformHooks"`
  first: an outdated plugin, harmless.
- **How the code is given:** as in Plan 9A — new files whole, changes as `git apply`
  patches pasted from the verified commit, files LF.
- Baseline (after Plan 9A): `cargo test --workspace --locked` passes 927 tests;
  `deploy/helm/test-chart.ps1` passes 31 checks.
- **Cargo.lock.** Task 1's patch carries the lockfile change (`push-smoke` depends on
  `push-sdk`); never run `cargo update`.
- **Rate limits.** A smoke run signs up five users (three raw, two SDK) from one IP; the
  per-IP signup limit is 20 an hour, so a long-lived cluster allows about four runs an
  hour.
- **Every task ends green** with the four gates of Plan 9A plus `pwsh deploy/helm/test-chart.ps1`.
- **Scope rule:** no change to `push-sdk` or any service; only the smoke test, its Pod, the
  chart test and the README.

## Key rules the configuration must preserve

- **The SDK apps are clients**: they reach auth-svc and the gateway only through the
  ingress, with the `Host` override, like the raw smoke test; only the DEV ONLY mailbox is
  read from the worker directly, as a browser would open the emailed links.
- **No secret in a Pod's arguments, no token in any log.** The smoke test prints ids, seqs,
  counts and timings only; the SDK never logs tokens. Verified on the cluster by grepping
  the smoke Pod's and the gateway's logs.
- **The smoke Pod's root filesystem stays read-only**: the SDK's databases go to an
  `emptyDir` with a size limit.
- **Every wait is bounded**: 10 s a step, 30 s for what waits for the worker, 300 s for the
  whole run.

## Decisions made for this plan

1. **The raw smoke test stays; the SDK scenario is added after it, in the same binary and
   Pod.** The raw half checks what the SDK deliberately hides and an app cannot do: exact
   frames and close codes, tokens forged with auth-svc's key (expired, wrong audience), the
   refused group request, `Ack` and `ReadUpTo` by hand. The SDK half checks the path every
   app takes: the SDK's own reconnects, outbox, acks, receipts and events against the real
   gateway. Replacing the raw half would lose the protocol checks; a separate SDK test run
   from the host would need port-forwards to the ingress and the worker and could not run
   in `helm test`. One run of `up.ps1 -Smoke` covers both, and CI's stack test
   (`crates/push-smoke/tests/local.rs`) runs both too.
2. **Restarts are real.** An app restart drops the `Client` (its runtime stops at once) and
   opens a new one on the same SQLite file and secret store; the outbox is read back from
   disk. The cut-off send is queued and the client dropped at once, so whether the first
   attempt reached the server is left to chance; either way the message must be stored
   once, which the `seq` of the next message proves (no duplicate entry in the log).
3. **The revocation and the password reset are the SDK's events.** Dana revokes her own
   device through the SDK (`DELETE /v1/devices/{id}`); the gateway closes with 4003 once
   the worker relays it, and the SDK reports `DeviceRemoved` and stops. Erin asks for a
   reset through the SDK, the test opens the link from the mailbox and sets a new
   password; the gateway closes with 4001 and the SDK reports `SignedOut` and forgets the
   credentials. Erin signs in again **at least 1.1 s later**: the gateway refuses a token
   issued in the same second as the reset (spec §5.3, within that second the order is
   unknown), so a sign-in within that second would be signed out again — found in this
   plan's verification; a person never signs in that fast. The SDK signs the same device in
   again (`login` with `device_id`), and the history is still there.
4. **`--data-dir` is required** (`PUSH_SMOKE_DATA_DIR`), like the other target settings;
   the chart passes `/data`. The smoke Pod's memory limit rises from 64 MiB to 128 MiB
   (two SDK clients with their runtimes and SQLite), its request from 16 to 32 MiB; the
   whole run may take 300 s instead of 180 s.

## Owner decisions

None new. Plan 9A's owner decisions needed (data on a server sign-out, takeover versus
revocation, default secure stores, database encryption) do not change this plan: the
scenario checks the behaviour as built.

## File structure

```
crates/push-smoke/src/sdk.rs           NEW: the SDK scenario
crates/push-smoke/src/lib.rs           --data-dir; the SDK scenario after the raw one
crates/push-smoke/src/auth.rs          verify an email and complete a reset as a browser does
crates/push-smoke/src/main.rs          300 s for the whole run
crates/push-smoke/Cargo.toml           + push-sdk
crates/push-smoke/tests/local.rs       a data directory for the SDK apps
Cargo.lock                             push-smoke -> push-sdk
deploy/helm/push-messenger/templates/tests/smoke.yaml   emptyDir at /data, --data-dir, memory
deploy/helm/test-chart.ps1             32 checks
README.md, roadmap                     docs
```

---

### Task 1: The SDK half of the smoke test

**Files:**
- Create: `crates/push-smoke/src/sdk.rs`
- Modify: `Cargo.lock`, `README.md`, `crates/push-smoke/Cargo.toml`, `crates/push-smoke/src/auth.rs`, `crates/push-smoke/src/lib.rs`, `crates/push-smoke/src/main.rs`, `crates/push-smoke/tests/local.rs`, `deploy/helm/push-messenger/templates/tests/smoke.yaml`, `deploy/helm/test-chart.ps1`

Decisions 1–4.

- [ ] **Step 1: The chart check first (red)**

Change `deploy/helm/test-chart.ps1`:

```diff
diff --git a/deploy/helm/test-chart.ps1 b/deploy/helm/test-chart.ps1
index bbb8e81..34546d5 100644
--- a/deploy/helm/test-chart.ps1
+++ b/deploy/helm/test-chart.ps1
@@ -158,6 +158,13 @@ Check 'minikube: the smoke test reaches only the ingress and the mailbox, with a
         $smoke -match 'name: PUSH_SMOKE_SIGNING_KEY\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: auth-signing-key' -and
         $key -and -not $smoke.Contains($key) -and $dev -notmatch 'PUSH_DEV_SETUP'
 }
+Check 'minikube: the smoke test keeps the SDK apps'' databases in an emptyDir' {
+    $smoke = Doc $dev 'Pod' 'push-smoke'
+    $smoke -match '- --data-dir\s+- /data' -and
+        $smoke -match 'volumeMounts:\s+- name: data\s+mountPath: /data' -and
+        $smoke -match 'volumes:\s+- name: data\s+emptyDir:' -and
+        $smoke -match 'readOnlyRootFilesystem: true'
+}
 Check 'minikube: the worker''s dev mailbox on' {
     (Doc $dev 'Deployment' 'push-worker') -match 'name: PUSH_DEV_MAILBOX\s+value: "1"'
 }
```

```powershell
pwsh deploy/helm/test-chart.ps1
```

Expected: `FAIL minikube: the smoke test keeps the SDK apps' databases in an emptyDir` and
`1 check(s) failed`.

- [ ] **Step 2: Make the changes**

Change `deploy/helm/push-messenger/templates/tests/smoke.yaml`:

```diff
diff --git a/deploy/helm/push-messenger/templates/tests/smoke.yaml b/deploy/helm/push-messenger/templates/tests/smoke.yaml
index 1045773..e844a01 100644
--- a/deploy/helm/push-messenger/templates/tests/smoke.yaml
+++ b/deploy/helm/push-messenger/templates/tests/smoke.yaml
@@ -3,7 +3,9 @@
 # API and the WebSockets go through the ingress controller with the ingress host as the
 # Host header, as a client's would; only the DEV ONLY mailbox is read straight from the
 # worker, which the ingress does not route. It gets auth-svc's signing key from the Secret,
-# to sign an expired token and one for another audience that the gateway must refuse.
+# to sign an expired token and one for another audience that the gateway must refuse. Its
+# second half runs two apps built on push-sdk, whose SQLite databases live in an emptyDir
+# (the root filesystem is read-only).
 apiVersion: v1
 kind: Pod
 metadata:
@@ -30,6 +32,11 @@ spec:
         - {{ .Values.smoke.socketUrl | quote }}
         - --host
         - {{ .Values.ingress.host | quote }}
+        - --data-dir
+        - /data
+      volumeMounts:
+        - name: data
+          mountPath: /data
       env:
         - name: PUSH_SMOKE_SIGNING_KEY
           valueFrom:
@@ -39,8 +46,12 @@ spec:
       resources:
         requests:
           cpu: 10m
-          memory: 16Mi
+          memory: 32Mi
         limits:
           cpu: 200m
-          memory: 64Mi
+          memory: 128Mi
+  volumes:
+    - name: data
+      emptyDir:
+        sizeLimit: 64Mi
 {{- end }}
```

Change `Cargo.lock`:

```diff
diff --git a/Cargo.lock b/Cargo.lock
index 1878deb..6cdc8ee 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -2236,6 +2236,7 @@ dependencies = [
  "push-gateway",
  "push-message-svc",
  "push-proto",
+ "push-sdk",
  "push-store",
  "push-worker",
  "reqwest",
```

Change `crates/push-smoke/Cargo.toml`:

```diff
diff --git a/crates/push-smoke/Cargo.toml b/crates/push-smoke/Cargo.toml
index 3ad4505..94d6434 100644
--- a/crates/push-smoke/Cargo.toml
+++ b/crates/push-smoke/Cargo.toml
@@ -14,6 +14,7 @@ base64.workspace = true
 futures-util.workspace = true
 jsonwebtoken.workspace = true
 push-proto.workspace = true
+push-sdk.workspace = true
 reqwest.workspace = true
 serde.workspace = true
 serde_json.workspace = true
```

Change `crates/push-smoke/src/auth.rs`:

```diff
diff --git a/crates/push-smoke/src/auth.rs b/crates/push-smoke/src/auth.rs
index 10f5649..b44135c 100644
--- a/crates/push-smoke/src/auth.rs
+++ b/crates/push-smoke/src/auth.rs
@@ -282,6 +282,13 @@ impl<'a> Auth<'a> {
             "ask for a password reset",
         )
         .await?;
+        self.complete_password_reset(email).await?;
+        Ok(())
+    }
+
+    /// Opens the newest reset link the worker delivered to `email` and sets a new
+    /// password, as the user does in a browser; the new password.
+    pub async fn complete_password_reset(&self, email: &str) -> anyhow::Result<String> {
         let (token, _) = self.mailbox_token(email, "reset_password").await?;
         let password = format!("smoke-{}", Uuid::now_v7());
         expect(
@@ -293,6 +300,19 @@ impl<'a> Auth<'a> {
             "reset the password",
         )
         .await?;
+        Ok(password)
+    }
+
+    /// Verifies `email` with the link the worker delivered, as the user does in a
+    /// browser (an app signed up through the SDK).
+    pub async fn verify_email(&self, email: &str) -> anyhow::Result<()> {
+        let (token, _) = self.mailbox_token(email, "verify_email").await?;
+        expect(
+            self.post("/v1/auth/verify", json!({"token": token})),
+            StatusCode::OK,
+            "verify the email",
+        )
+        .await?;
         Ok(())
     }
 
```

Change `crates/push-smoke/src/lib.rs`:

```diff
diff --git a/crates/push-smoke/src/lib.rs b/crates/push-smoke/src/lib.rs
index 53514e2..72c86bd 100644
--- a/crates/push-smoke/src/lib.rs
+++ b/crates/push-smoke/src/lib.rs
@@ -22,12 +22,17 @@
 //!    (4002, 4001);
 //! 7. Carol revokes her device and Bob resets his password through auth-svc; once the
 //!    worker relays them, message-svc kicks both devices and the gateway closes Carol's
-//!    socket with 4003 and Bob's with 4001, and refuses their old tokens from then on.
+//!    socket with 4003 and Bob's with 4001, and refuses their old tokens from then on;
+//! 8. Dana and Erin do it all again as apps built on `push-sdk` (`sdk.rs`): signing up and
+//!    in, an invite, a chat with receipts, an edit and a delete, an outbox that survives
+//!    restarts, and the SDK's `DeviceRemoved` and `SignedOut`.
 //!
 //! The deployment must run the worker with the log sender and `PUSH_DEV_MAILBOX=1`. The
-//! smoke test needs auth-svc's signing key (from the deployment's Secret) for step 6.
+//! smoke test needs auth-svc's signing key (from the deployment's Secret) for step 6 and
+//! a writable directory for the SDK's databases.
 
 mod auth;
+mod sdk;
 mod socket;
 mod tokens;
 
@@ -72,6 +77,8 @@ pub struct Target {
     /// auth-svc's signing key (base64 of the 32-byte Ed25519 seed), to sign tokens the
     /// gateway must refuse. It can sign in as anyone, so it is never shown.
     pub signing_key: String,
+    /// A writable directory for the SDK apps' databases.
+    pub data_dir: String,
 }
 
 impl std::fmt::Debug for Target {
@@ -83,23 +90,26 @@ impl std::fmt::Debug for Target {
             .field("socket_url", &self.socket_url)
             .field("host", &self.host)
             .field("signing_key", &"<redacted>")
+            .field("data_dir", &self.data_dir)
             .finish()
     }
 }
 
 /// The usage line of the `push-smoke` binary.
-pub const USAGE: &str = "usage: push-smoke --auth <url> --mailbox <url> --socket <ws-url> [--host <host>], with auth-svc's signing key in PUSH_SMOKE_SIGNING_KEY";
+pub const USAGE: &str = "usage: push-smoke --auth <url> --mailbox <url> --socket <ws-url> --data-dir <dir> [--host <host>], with auth-svc's signing key in PUSH_SMOKE_SIGNING_KEY";
 
 impl Target {
-    /// Reads `--auth`, `--mailbox`, `--socket` and `--host` from `args`; each one missing
-    /// there is read from `env` (`PUSH_SMOKE_AUTH_URL`, `PUSH_SMOKE_MAILBOX_URL`,
-    /// `PUSH_SMOKE_SOCKET_URL`, `PUSH_SMOKE_HOST`). The signing key comes from
+    /// Reads `--auth`, `--mailbox`, `--socket`, `--data-dir` and `--host` from `args`;
+    /// each one missing there is read from `env` (`PUSH_SMOKE_AUTH_URL`,
+    /// `PUSH_SMOKE_MAILBOX_URL`, `PUSH_SMOKE_SOCKET_URL`, `PUSH_SMOKE_DATA_DIR`,
+    /// `PUSH_SMOKE_HOST`). The signing key comes from
     /// `PUSH_SMOKE_SIGNING_KEY` only: a secret never belongs in a Pod's arguments.
     pub fn parse(
         args: impl IntoIterator<Item = String>,
         env: impl Fn(&str) -> Option<String>,
     ) -> Result<Self, String> {
         let (mut auth_url, mut mailbox_url, mut socket_url, mut host) = (None, None, None, None);
+        let mut data_dir = None;
         let mut args = args.into_iter();
         while let Some(flag) = args.next() {
             let slot = match flag.as_str() {
@@ -107,6 +117,7 @@ impl Target {
                 "--mailbox" => &mut mailbox_url,
                 "--socket" => &mut socket_url,
                 "--host" => &mut host,
+                "--data-dir" => &mut data_dir,
                 other => return Err(format!("unknown argument {other:?}; {USAGE}")),
             };
             *slot = Some(args.next().ok_or_else(|| format!("{flag} needs a value"))?);
@@ -137,6 +148,11 @@ impl Target {
                 .or_else(|| env("PUSH_SMOKE_HOST"))
                 .filter(|host| !host.is_empty()),
             signing_key: required(None, "PUSH_SMOKE_SIGNING_KEY", "PUSH_SMOKE_SIGNING_KEY")?,
+            data_dir: required(
+                data_dir,
+                "--data-dir or PUSH_SMOKE_DATA_DIR",
+                "PUSH_SMOKE_DATA_DIR",
+            )?,
         })
     }
 }
@@ -152,8 +168,8 @@ struct Client {
     conn: Box<Conn>,
 }
 
-/// Runs the smoke test, printing each step. Fails at the first step that does not
-/// complete within 10 s (30 s for what waits for the worker).
+/// Runs the smoke test, printing each step: the raw protocol, then the SDK. Fails at the
+/// first step that does not complete within 10 s (30 s for what waits for the worker).
 pub async fn run(target: &Target) -> anyhow::Result<()> {
     let started = Instant::now();
     let auth = auth::Auth::connect(target, STEP_TIMEOUT).await?;
@@ -176,6 +192,7 @@ pub async fn run(target: &Target) -> anyhow::Result<()> {
     refuse_tokens(target, &auth, &forger, &alice).await?;
     kick(target, &auth, bob, carol).await?;
     alice.conn.close().await;
+    sdk::run(target, &auth).await?;
     step(&format!("PASS in {} ms", started.elapsed().as_millis()));
     Ok(())
 }
@@ -674,6 +691,8 @@ mod tests {
                 "ws://ingress/v1/socket",
                 "--host",
                 "push.local",
+                "--data-dir",
+                "/data",
             ]),
             key,
         );
@@ -685,6 +704,7 @@ mod tests {
                 socket_url: "ws://ingress/v1/socket".to_owned(),
                 host: Some("push.local".to_owned()),
                 signing_key: "c2VjcmV0LWtleQ==".to_owned(),
+                data_dir: "/data".to_owned(),
             })
         );
     }
@@ -696,6 +716,7 @@ mod tests {
             |name| match name {
                 "PUSH_SMOKE_AUTH_URL" => Some("http://auth:8080".to_owned()),
                 "PUSH_SMOKE_MAILBOX_URL" => Some("http://worker:8080".to_owned()),
+                "PUSH_SMOKE_DATA_DIR" => Some("/data".to_owned()),
                 other => key(other),
             },
         )
@@ -714,6 +735,7 @@ mod tests {
             "PUSH_SMOKE_MAILBOX_URL",
             "PUSH_SMOKE_SOCKET_URL",
             "PUSH_SMOKE_SIGNING_KEY",
+            "PUSH_SMOKE_DATA_DIR",
         ] {
             let error =
                 Target::parse(args(&[]), |name| all(name).filter(|_| name != missing)).unwrap_err();
```

Change `crates/push-smoke/src/main.rs`:

```diff
diff --git a/crates/push-smoke/src/main.rs b/crates/push-smoke/src/main.rs
index 82f1f13..6a3398f 100644
--- a/crates/push-smoke/src/main.rs
+++ b/crates/push-smoke/src/main.rs
@@ -4,7 +4,7 @@ use std::time::Duration;
 use push_smoke::{Target, run};
 
 /// The whole run, so that a hung deployment fails a helm test instead of hanging it.
-const TOTAL_TIMEOUT: Duration = Duration::from_secs(180);
+const TOTAL_TIMEOUT: Duration = Duration::from_secs(300);
 
 #[tokio::main]
 async fn main() -> ExitCode {
```

Create `crates/push-smoke/src/sdk.rs`:

```rust
//! The SDK half of the smoke test: two apps built on `push-sdk`, as a third party builds
//! them, against the same deployment and through the same ingress.
//!
//! Dana and Erin sign up and in with the SDK (the gateway answers 4006 until the worker
//! relays a device, and the SDK reconnects by itself), become contacts by invite and chat:
//! a message, its delivery and read receipt, an edit and a delete. Erin queues messages
//! offline and restarts her app: the outbox sends them after the restart, in order and
//! once, and a send cut off by a restart is stored once. Finally Dana revokes her own
//! device and Erin resets her password: the SDK reports `DeviceRemoved` (4003) and
//! `SignedOut` (4001), and Erin signs in again with the same device and her history.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use push_sdk::{
    Client, ClientMsgId, Config, ConnectionState, ContactState, ConversationId, Event, Events,
    MemorySecretStore, MessageStatus, Platform, UserId,
};
use uuid::Uuid;

use crate::auth::Auth;
use crate::{STEP_TIMEOUT, Target, WORKER_TIMEOUT};

/// One app: a client with its own database and secret store.
struct App {
    name: &'static str,
    email: String,
    password: String,
    user: UserId,
    database: String,
    secrets: Arc<MemorySecretStore>,
    config: Config,
    client: Client,
    events: Events,
    /// Events received while the test waited for another.
    pending: VecDeque<Event>,
}

impl App {
    /// Signs `name` up with the SDK, verifies the address with the link the worker
    /// delivered, signs in (registering the device) and connects.
    async fn enroll(target: &Target, auth: &Auth<'_>, name: &'static str) -> anyhow::Result<Self> {
        let started = Instant::now();
        let suffix = Uuid::now_v7().simple().to_string();
        let suffix = &suffix[suffix.len() - 10..];
        let email = format!("{name}.{suffix}@smoke.test");
        let password = format!("smoke-{}", Uuid::now_v7());
        let database = format!(
            "{}/{name}-{suffix}.db",
            target.data_dir.trim_end_matches('/')
        );
        let mut config = Config::new(&target.auth_url, &target.socket_url, &database);
        config.host = target.host.clone();
        let secrets = Arc::new(MemorySecretStore::new());
        let (client, events) = Client::open(config.clone(), secrets.clone())?;
        client
            .sign_up(&email, &password, &format!("{name}_{suffix}"), name)
            .await
            .context("sign up with the SDK")?;
        auth.verify_email(&email).await?;
        let account = client
            .sign_in(&email, &password, &format!("{name}'s app"), Platform::Other)
            .await
            .context("sign in with the SDK")?;
        let mut app = Self {
            name,
            email,
            password,
            user: account.user_id,
            database,
            secrets,
            config,
            client,
            events,
            pending: VecDeque::new(),
        };
        let retries = app.connect().await?;
        sdk_step(&format!(
            "{name} signed up and in with push-sdk and connected (device {}, {retries} reconnects while the device was not relayed yet) in {} ms",
            account.device_id,
            started.elapsed().as_millis()
        ));
        Ok(app)
    }

    /// Connects and waits for `Connected`; how many times the SDK had to retry.
    async fn connect(&mut self) -> anyhow::Result<usize> {
        self.client.connect()?;
        let mut retries = 0;
        let deadline = tokio::time::Instant::now() + WORKER_TIMEOUT;
        loop {
            let event = self.next_event(deadline, "Connected").await?;
            match event {
                Event::ConnectionStateChanged(ConnectionState::Connected) => return Ok(retries),
                Event::ConnectionStateChanged(ConnectionState::Reconnecting { .. }) => retries += 1,
                Event::ConnectionStateChanged(
                    state @ (ConnectionState::SignedOut | ConnectionState::DeviceRemoved),
                ) => bail!("{}: {state:?} while connecting", self.name),
                other => self.pending.push_back(other),
            }
        }
    }

    async fn next_event(
        &mut self,
        deadline: tokio::time::Instant,
        waiting_for: &str,
    ) -> anyhow::Result<Event> {
        tokio::time::timeout_at(deadline, self.events.next())
            .await
            .map_err(|_| anyhow::anyhow!("{}: no {waiting_for} in time", self.name))?
            .with_context(|| format!("{}: the client is gone", self.name))
    }

    /// The first event `wanted` accepts, kept or new, within `timeout`; the others are
    /// kept. A `SendFailed` that no wait accepts fails the test.
    async fn wait_for(
        &mut self,
        waiting_for: &str,
        timeout: Duration,
        wanted: impl Fn(&Event) -> bool,
    ) -> anyhow::Result<Event> {
        if let Some(index) = self.pending.iter().position(&wanted) {
            return Ok(self.pending.remove(index).expect("in range"));
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let event = self.next_event(deadline, waiting_for).await?;
            if wanted(&event) {
                return Ok(event);
            }
            if let Event::SendFailed { error, .. } = &event {
                bail!(
                    "{}: a send failed while waiting for {waiting_for}: {}",
                    self.name,
                    error.code()
                );
            }
            self.pending.push_back(event);
        }
    }

    /// Ends the app as a process exit would, and starts it again on the same database
    /// and secret store.
    fn restart(&mut self) -> anyhow::Result<()> {
        let (client, events) = Client::open(self.config.clone(), self.secrets.clone())?;
        self.client = client;
        self.events = events;
        self.pending.clear();
        Ok(())
    }
}

/// Runs the SDK scenario.
pub(crate) async fn run(target: &Target, auth: &Auth<'_>) -> anyhow::Result<()> {
    let mut dana = App::enroll(target, auth, "dana").await?;
    let mut erin = App::enroll(target, auth, "erin").await?;

    let invite = dana.client.create_invite(600, 1).await?;
    let creator = erin.client.redeem_invite(&invite.code).await?;
    if creator != dana.user {
        bail!("erin's redeem named someone other than dana");
    }
    let erin_user = erin.user;
    dana.wait_for("the new contact", STEP_TIMEOUT, |event| {
        matches!(event, Event::ContactChanged(contact)
            if contact.user_id == erin_user && contact.state == ContactState::Accepted)
    })
    .await?;
    sdk_step("erin redeemed dana's invite; dana's app heard the new contact");

    let chat = dana.client.open_direct(erin.user).await?.id;
    let (seq, sent_id) = chat_and_read(&mut dana, &mut erin, chat).await?;
    edit_and_delete(&mut dana, &mut erin, chat, sent_id).await?;
    let next = restart_resumes_the_outbox(&mut dana, &mut erin, chat, seq).await?;
    cut_off_send_is_stored_once(&mut dana, &mut erin, chat, next).await?;
    removed_and_signed_out(auth, &mut dana, &mut erin, chat).await?;
    let _ = (dana.database, erin.database, dana.password);
    Ok(())
}

/// Dana sends, Erin's app receives and reads, Dana's app shows it read.
async fn chat_and_read(
    dana: &mut App,
    erin: &mut App,
    chat: ConversationId,
) -> anyhow::Result<(i64, ClientMsgId)> {
    let started = Instant::now();
    let text = format!("hello from push-sdk {}", Uuid::now_v7());
    let echo = dana.client.send(chat, &text)?;
    if echo.status != MessageStatus::Pending {
        bail!("the local echo is {:?}, not Pending", echo.status);
    }
    let id = echo.client_msg_id;
    let Event::MessageSent(sent) = dana
        .wait_for(
            "dana's MessageSent",
            STEP_TIMEOUT,
            |event| matches!(event, Event::MessageSent(message) if message.client_msg_id == id),
        )
        .await?
    else {
        unreachable!("the wait accepts only MessageSent");
    };
    let seq = sent.seq.context("a sent message has a seq")?;
    let Event::MessageAdded(received) = erin
        .wait_for(
            "erin's MessageAdded",
            STEP_TIMEOUT,
            |event| matches!(event, Event::MessageAdded(message) if message.client_msg_id == id),
        )
        .await?
    else {
        unreachable!("the wait accepts only MessageAdded");
    };
    if received.body.as_deref() != Some(text.as_str()) || received.status != MessageStatus::Received
    {
        bail!(
            "erin's app received the wrong message (seq {:?})",
            received.seq
        );
    }
    sdk_step(&format!(
        "dana's message was stored (seq {seq}) and erin's app received it after {} ms",
        started.elapsed().as_millis()
    ));

    erin.client.mark_read(chat, seq)?;
    dana.wait_for("the read receipt", STEP_TIMEOUT, |event| {
        matches!(event, Event::ReceiptChanged { conversation_id, peer_read_seq, .. }
            if *conversation_id == chat && *peer_read_seq >= seq)
    })
    .await?;
    let status = dana
        .client
        .messages(chat, None, 50)?
        .into_iter()
        .find(|message| message.client_msg_id == id)
        .map(|message| message.status);
    if status != Some(MessageStatus::Read) {
        bail!("dana's app shows the message as {status:?}, not Read");
    }
    sdk_step("erin read it; dana's app shows it read");
    Ok((seq, id))
}

async fn edit_and_delete(
    dana: &mut App,
    erin: &mut App,
    chat: ConversationId,
    id: ClientMsgId,
) -> anyhow::Result<()> {
    dana.client.edit(chat, id, "hello, edited")?;
    erin.wait_for("erin's MessageEdited", STEP_TIMEOUT, |event| {
        matches!(event, Event::MessageEdited(message)
            if message.client_msg_id == id && message.body.as_deref() == Some("hello, edited"))
    })
    .await?;
    dana.client.delete(chat, id)?;
    erin.wait_for(
        "erin's MessageDeleted",
        STEP_TIMEOUT,
        |event| matches!(event, Event::MessageDeleted(message) if message.client_msg_id == id),
    )
    .await?;
    let held = erin
        .client
        .messages(chat, None, 50)?
        .into_iter()
        .find(|message| message.client_msg_id == id)
        .context("erin's app no longer holds the message")?;
    if !held.deleted || held.body.is_some() {
        bail!("erin's app still holds the deleted message's body");
    }
    sdk_step("dana edited and deleted it; erin's app applied both and holds no body");
    Ok(())
}

/// Erin queues three messages offline and restarts her app; once connected the outbox
/// sends them in order. The `seq` the next message gets.
async fn restart_resumes_the_outbox(
    dana: &mut App,
    erin: &mut App,
    chat: ConversationId,
    after: i64,
) -> anyhow::Result<i64> {
    erin.client.disconnect()?;
    erin.wait_for("Offline", STEP_TIMEOUT, |event| {
        *event == Event::ConnectionStateChanged(ConnectionState::Offline)
    })
    .await?;
    let mut queued = Vec::new();
    for n in 1..=3 {
        queued.push(
            erin.client
                .send(chat, &format!("queued offline {n}"))?
                .client_msg_id,
        );
    }
    erin.restart()?;
    let pending = erin
        .client
        .messages(chat, None, 50)?
        .iter()
        .filter(|message| message.status == MessageStatus::Pending)
        .count();
    if pending != 3 {
        bail!("after the restart erin's outbox holds {pending} messages, not 3");
    }
    erin.connect().await?;
    let mut seqs = Vec::new();
    for id in &queued {
        let Event::MessageAdded(message) = dana
            .wait_for("a queued message", STEP_TIMEOUT, |event| {
                matches!(event, Event::MessageAdded(message) if message.client_msg_id == *id)
            })
            .await?
        else {
            unreachable!("the wait accepts only MessageAdded");
        };
        seqs.push(message.seq.context("a delivered message has a seq")?);
    }
    // The edit and the delete took after + 1 and after + 2.
    let first = after + 3;
    if seqs != [first, first + 1, first + 2] {
        bail!("the queued messages arrived as {seqs:?}, not in order from {first}");
    }
    for id in &queued {
        erin.wait_for(
            "erin's MessageSent",
            STEP_TIMEOUT,
            |event| matches!(event, Event::MessageSent(message) if message.client_msg_id == *id),
        )
        .await?;
    }
    sdk_step(&format!(
        "erin queued 3 messages offline and restarted her app; once connected the outbox sent them in order (seq {}..{}) and dana's app received each once",
        seqs[0], seqs[2]
    ));
    Ok(first + 3)
}

/// A send whose app ends at once is retried after the restart with the same
/// `client_msg_id` and stored once: the following message takes the next `seq`.
async fn cut_off_send_is_stored_once(
    dana: &mut App,
    erin: &mut App,
    chat: ConversationId,
    expected: i64,
) -> anyhow::Result<()> {
    let id = erin
        .client
        .send(chat, "cut off by a restart")?
        .client_msg_id;
    erin.restart()?;
    erin.connect().await?;
    let Event::MessageSent(sent) = erin
        .wait_for(
            "the retried MessageSent",
            STEP_TIMEOUT,
            |event| matches!(event, Event::MessageSent(message) if message.client_msg_id == id),
        )
        .await?
    else {
        unreachable!("the wait accepts only MessageSent");
    };
    let next = erin
        .client
        .send(chat, "after the cut-off send")?
        .client_msg_id;
    let Event::MessageAdded(following) = dana
        .wait_for(
            "the following message",
            STEP_TIMEOUT,
            |event| matches!(event, Event::MessageAdded(message) if message.client_msg_id == next),
        )
        .await?
    else {
        unreachable!("the wait accepts only MessageAdded");
    };
    if sent.seq != Some(expected) || following.seq != Some(expected + 1) {
        bail!(
            "the cut-off send took {:?} and the next {:?}, not {expected} and {}",
            sent.seq,
            following.seq,
            expected + 1
        );
    }
    sdk_step(&format!(
        "a send cut off by a restart was retried with its client_msg_id and stored once (seq {expected}; the next took {})",
        expected + 1
    ));
    Ok(())
}

/// Dana revokes her device and Erin resets her password: the SDK reports
/// `DeviceRemoved` and `SignedOut`, and Erin signs in again with the same device.
async fn removed_and_signed_out(
    auth: &Auth<'_>,
    dana: &mut App,
    erin: &mut App,
    chat: ConversationId,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let device = dana
        .client
        .account()?
        .context("dana is signed in")?
        .device_id;
    dana.client.revoke_device(device).await?;
    dana.wait_for("DeviceRemoved", WORKER_TIMEOUT, |event| {
        *event == Event::DeviceRemoved
    })
    .await?;
    if dana.client.connection_state() != ConnectionState::DeviceRemoved {
        bail!(
            "dana's app is {:?}, not DeviceRemoved",
            dana.client.connection_state()
        );
    }
    sdk_step(&format!(
        "dana revoked her device; her app reported DeviceRemoved (4003) after {} ms and stopped",
        started.elapsed().as_millis()
    ));

    let started = Instant::now();
    let device = erin
        .client
        .account()?
        .context("erin is signed in")?
        .device_id;
    erin.client.forgot_password(&erin.email).await?;
    erin.password = auth.complete_password_reset(&erin.email).await?;
    erin.wait_for("SignedOut", WORKER_TIMEOUT, |event| {
        *event == Event::SignedOut
    })
    .await?;
    let waited = started.elapsed();
    if erin.client.account()?.is_some() {
        bail!("erin's app still holds credentials after SignedOut");
    }
    // The gateway refuses a token issued in the same second as the reset (spec §5.3:
    // within that second the order is unknown), so a person signing in again within a
    // second is signed out once more. A person takes longer; the test waits.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let account = erin
        .client
        .sign_in(&erin.email, &erin.password, "erin's app", Platform::Other)
        .await?;
    if account.device_id != device {
        bail!("erin's app registered a new device instead of signing the same one in");
    }
    erin.connect().await?;
    let history = erin.client.messages(chat, None, 50)?.len();
    if history == 0 {
        bail!("erin's app lost its history");
    }
    sdk_step(&format!(
        "erin reset her password; her app reported SignedOut (4001) after {} ms, signed the same device in again and kept its {history} messages",
        waited.as_millis()
    ));
    erin.client.disconnect()?;
    Ok(())
}

fn sdk_step(message: &str) {
    println!("smoke: sdk: {message}");
}
```

Change `crates/push-smoke/tests/local.rs`:

```diff
diff --git a/crates/push-smoke/tests/local.rs b/crates/push-smoke/tests/local.rs
index 423a543..40c70fc 100644
--- a/crates/push-smoke/tests/local.rs
+++ b/crates/push-smoke/tests/local.rs
@@ -174,16 +174,21 @@ async fn the_smoke_test_passes_against_the_whole_stack() {
     wait_until_ready("message-svc", message_svc_health_addr, &message_svc).await;
     wait_until_ready("the gateway", gateway_health_addr, &gateway).await;
 
+    // The SDK apps' databases, fresh for every run.
+    let data_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
+        .join(format!("push-smoke-{}", uuid::Uuid::now_v7()));
+    std::fs::create_dir_all(&data_dir).unwrap();
     let target = Target {
         mailbox_url: worker_url,
         auth_url,
         socket_url,
         host: None,
         signing_key: signing_key.to_owned(),
+        data_dir: data_dir.to_string_lossy().into_owned(),
     };
-    tokio::time::timeout(Duration::from_secs(180), run(&target))
+    tokio::time::timeout(Duration::from_secs(300), run(&target))
         .await
-        .expect("the smoke test finishes within 120 s")
+        .expect("the smoke test finishes within 300 s")
         .expect("the smoke test passes");
 
     shutdown.cancel();
@@ -194,6 +199,7 @@ async fn the_smoke_test_passes_against_the_whole_stack() {
     gateway.await.unwrap().expect("the gateway stops cleanly");
     auth.await.unwrap().expect("auth-svc stops cleanly");
     worker.await.unwrap().expect("the worker stops cleanly");
+    let _ = std::fs::remove_dir_all(&data_dir);
 }
 
 /// Polls `/readyz` of a server task until it answers 200; fails if the task stops first.
```

Change `README.md`:

```diff
diff --git a/README.md b/README.md
index ac76fc4..deb6c4d 100644
--- a/README.md
+++ b/README.md
@@ -53,7 +53,11 @@ register a device and connect its WebSocket with the access token (the gateway a
 become contacts by request and by invite, chat, edit, read and delete, see a group request
 refused, and see their allowed profiles from auth-svc; the gateway refuses an expired token
 and one for another audience; finally a revoked device and a password reset close their
-sockets with 4003 and 4001. Only the DEV ONLY mailbox is read from the worker directly.
+sockets with 4003 and 4001. Then two apps built on `push-sdk` do the same through the
+SDK: they sign up and in, chat with receipts, edit and delete, restart with messages in
+their outbox (sent in order, once), and see `DeviceRemoved` after a revocation and
+`SignedOut` after a password reset. Only the DEV ONLY mailbox is read from the worker
+directly.
 `deploy/minikube/up.sh --smoke` does the same on Linux
 and macOS. Both wait for ingress-nginx, retry
 `helm upgrade --install` up to 5 times while its admission webhook comes up, and
```

```powershell
pwsh deploy/helm/test-chart.ps1
```

Expected: `all chart checks passed` (32 `ok` lines).

- [ ] **Step 3: Run the whole scenario against the local stack**

```powershell
cargo test -p push-smoke --test local --locked -- --nocapture
```

Expected (verified 2026-10-14; ids and timings differ): the raw lines of Plan 8B, then

```
smoke: sdk: dana signed up and in with push-sdk and connected (device 01a13bb0-727b-7335-be52-e40ffde7b13f, 1 reconnects while the device was not relayed yet) in 883 ms
smoke: sdk: erin signed up and in with push-sdk and connected (device 01a13bb0-75d8-778b-af41-a6783b8a0a89, 1 reconnects while the device was not relayed yet) in 693 ms
smoke: sdk: erin redeemed dana's invite; dana's app heard the new contact
smoke: sdk: dana's message was stored (seq 1) and erin's app received it after 16 ms
smoke: sdk: erin read it; dana's app shows it read
smoke: sdk: dana edited and deleted it; erin's app applied both and holds no body
smoke: sdk: erin queued 3 messages offline and restarted her app; once connected the outbox sent them in order (seq 4..6) and dana's app received each once
smoke: sdk: a send cut off by a restart was retried with its client_msg_id and stored once (seq 7; the next took 8)
smoke: sdk: dana revoked her device; her app reported DeviceRemoved (4003) after 271 ms and stopped
smoke: sdk: erin reset her password; her app reported SignedOut (4001) after 792 ms, signed the same device in again and kept its 6 messages
smoke: PASS in 6344 ms
test the_smoke_test_passes_against_the_whole_stack ... ok
```

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
pwsh deploy/helm/test-chart.ps1
git add -A Cargo.lock README.md crates deploy
git commit -m "push-smoke: the SDK scenario; the smoke Pod keeps its databases in an emptyDir"
```

Expected: 927 tests pass (the stack test is still one test), no warnings, no diff, 32 chart
checks.

---

### Task 2: Bring it up on minikube and run the smoke test

**Files:** none (a verification task; commit nothing unless a fix is needed).

- [ ] **Step 1: Note the kubectl context**

```powershell
kubectl config current-context
```

Expected: `minikube` (restore it in Step 5).

- [ ] **Step 2: Bring it up and run the smoke test**

```powershell
deploy/minikube/up.ps1 -Smoke
```

Expected (verified 2026-10-14 as an upgrade of the stopped `push` profile, revision 15;
ids and timings differ): the five images build (the builder compiles `rusqlite` and SQLite
in about 2 minutes the first time), the release upgrades, and `helm test` ends
`Phase: Succeeded` with these pod logs:

```
==> start minikube profile 'push'
==> enable the ingress addon
==> wait for ingress-nginx
==> wait for the ingress-nginx admission jobs
==> check that the Postgres volume has the push_auth and push_worker databases
==> build push-messenger/auth:dev
==> build push-messenger/message-svc:dev
==> build push-messenger/gateway:dev
==> build push-messenger/worker:dev
==> build push-messenger/smoke:dev
==> install release 'push' in namespace 'push' (attempt 1 of 5)
Release "push" has been upgraded. Happy Helming!
STATUS: deployed
REVISION: 15
==> wait for the old worker pod to go away
==> wait for the worker pod to be Ready
==> run the smoke test
TEST SUITE:     push-smoke
Phase:          Succeeded
POD LOGS: push-smoke
smoke: alice signed up; the worker's log sender delivered the verification email after 1009 ms
smoke: alice verified the email, signed in and registered device 01a13baa-edf6-725e-8085-717934122283 (a retry returned the same device) in 1076 ms
smoke: alice's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: bob signed up; the worker's log sender delivered the verification email after 1007 ms
smoke: bob verified the email, signed in and registered device 01a13baa-f224-7671-bb7b-b7c7c4264200 (a retry returned the same device) in 1063 ms
smoke: bob's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: carol signed up; the worker's log sender delivered the verification email after 1008 ms
smoke: carol verified the email, signed in and registered device 01a13baa-f64e-7133-b7f3-074fd100278d (a retry returned the same device) in 1062 ms
smoke: carol's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: alice connected with the access token (Ready, heartbeat 30000 ms) once the worker had relayed device 01a13baa-edf6-725e-8085-717934122283: 0 "not ready yet" closes (4006) in 48 ms
smoke: bob connected with the access token (Ready, heartbeat 30000 ms) once the worker had relayed device 01a13baa-f224-7671-bb7b-b7c7c4264200: 0 "not ready yet" closes (4006) in 47 ms
smoke: carol connected with the access token (Ready, heartbeat 30000 ms) once the worker had relayed device 01a13baa-f64e-7133-b7f3-074fd100278d: 2 "not ready yet" closes (4006) in 766 ms
smoke: alice asked bob to be a contact; bob's socket heard it (ContactEvent), bob accepted and alice's socket heard that
smoke: carol redeemed alice's invite: they are contacts
smoke: alice opened the direct conversation with bob
smoke: auth-svc shows alice the profiles of her contacts bob and carol, and bob none of carol's
smoke: alice got SendAck seq=1 after 9 ms
smoke: bob got Deliver seq=1 after 54 ms
smoke: bob acked; the gateway answered the following Ping
smoke: alice edited her message; bob's socket delivered the edit (seq 2)
smoke: bob read the message; his socket heard his ReadPosition, alice's the Receipt, and her listing shows it read
smoke: alice's group request was refused (NOT_AVAILABLE): groups are not offered to apps
smoke: alice deleted her message; bob's socket delivered the delete (seq 3) and his history holds it erased
smoke: the gateway refused a token that had expired (4002) and one for another audience (4001), both signed with auth-svc's key
smoke: carol revoked her device; after the worker relayed it the gateway closed her socket with 4003 (522 ms) and refuses her token
smoke: bob reset his password; after the worker relayed it the gateway closed his socket with 4001 (2004 ms) and refuses his token from before the reset
smoke: sdk: dana signed up and in with push-sdk and connected (device 01a13bab-0921-7024-bc70-bdd70504b1d2, 2 reconnects while the device was not relayed yet) in 2051 ms
smoke: sdk: erin signed up and in with push-sdk and connected (device 01a13bab-1123-72e8-9034-87330c48e781, 2 reconnects while the device was not relayed yet) in 2302 ms
smoke: sdk: erin redeemed dana's invite; dana's app heard the new contact
smoke: sdk: dana's message was stored (seq 1) and erin's app received it after 10 ms
smoke: sdk: erin read it; dana's app shows it read
smoke: sdk: dana edited and deleted it; erin's app applied both and holds no body
smoke: sdk: erin queued 3 messages offline and restarted her app; once connected the outbox sent them in order (seq 4..6) and dana's app received each once
smoke: sdk: a send cut off by a restart was retried with its client_msg_id and stored once (seq 7; the next took 8)
smoke: sdk: dana revoked her device; her app reported DeviceRemoved (4003) after 69 ms and stopped
smoke: sdk: erin reset her password; her app reported SignedOut (4001) after 2010 ms, signed the same device in again and kept its 6 messages
smoke: PASS in 15143 ms
```

- [ ] **Step 3: Again, as an upgrade without building, and no token in the logs**

```powershell
deploy/minikube/up.ps1 -SkipBuild -Smoke
$logs = kubectl --context push -n push logs push-smoke
"smoke pod token-like: $(($logs | Select-String -SimpleMatch 'eyJ').Count), bearer: $(($logs | Select-String -SimpleMatch 'Bearer').Count)"
$gateway = kubectl --context push -n push logs deploy/push-gateway --tail=600
"gateway token-like: $(($gateway | Select-String -SimpleMatch 'eyJ').Count), bearer: $(($gateway | Select-String -SimpleMatch 'Bearer').Count)"
"kicks: $(($gateway | Select-String 'connection kicked').Count), frame budget closes: $(($gateway | Select-String 'too many frames').Count)"
```

Expected (verified 2026-10-14): `REVISION: 16`, `Phase: Succeeded`,
`smoke: PASS in 15400 ms` (or similar); `smoke pod token-like: 0, bearer: 0`,
`gateway token-like: 0, bearer: 0`, and `kicks: 8, frame budget closes: 0` after the two
runs (each run kicks four devices: Carol and Bob over raw frames, Dana and Erin through the
SDK; the SDK never exceeded the gateway's frame budget).

- [ ] **Step 4: If anything fails**

`kubectl --context push -n push get pods`, `kubectl --context push -n push logs
deploy/push-gateway` (or `push-message-svc`, `push-auth`, `push-worker`) and
`helm test push -n push --logs` show where. A smoke Pod killed for memory shows
`OOMKilled` in `kubectl --context push -n push describe pod push-smoke`. Fix it in the task
that introduced it, run that task's gates, and repeat Step 2.

- [ ] **Step 5: Stop the cluster and restore the context**

```powershell
minikube stop -p push
kubectl config use-context minikube
kubectl config current-context
```

Expected: `1 node stopped.` and `minikube`. Never delete the profile.

---

### Task 3: Roadmap and pull request

**Files:**
- Modify: `docs/superpowers/plans/2026-10-11-roadmap.md`

CI needs no change: the `images` job builds the five images (the smoke image now compiles
SQLite), the `chart` job runs `test-chart.ps1` with 32 checks, and the `rust` job runs the
stack test with the SDK half. The minikube run stays the merge gate, recorded in the pull
request.

- [ ] **Step 1: Mark the roadmap**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, set the last column of the 9B row to
`In review`.

- [ ] **Step 2: Final gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
pwsh deploy/helm/test-chart.ps1
git add -A docs
git commit -m "docs: mark plan 9B in review"
```

Expected: 927 tests pass, no warnings, no diff, `all chart checks passed`.

- [ ] **Step 3: Push, open the pull request and wait for CI**

```powershell
git push -u origin feature/plan-09b-sdk-minikube
$body = @'
Implements docs/superpowers/plans/2026-10-15-plan-09b-sdk-minikube.md: the SDK end to end. After the raw-protocol smoke test, two apps built on push-sdk run against the whole stack through the ingress: sign-up and sign-in (the SDK rides out 4006 by itself), an invite, a message with delivery and read receipts, an edit and a delete, an outbox that survives an app restart (sent in order, once), a send cut off by a restart stored once, DeviceRemoved after a revocation (4003) and SignedOut after a password reset (4001) with the same device signed in again and its history kept. The smoke Pod keeps the SDK's SQLite files in an emptyDir (root filesystem still read-only). 927 tests; test-chart 32 checks.

Verified on minikube (profile push, Kubernetes 1.31): up.ps1 -Smoke passed as an upgrade (revision 15, PASS in 15143 ms) and again with -SkipBuild (revision 16, PASS in 15400 ms); no token in the smoke Pod's or the gateway's logs; no frame-budget close.
'@
gh pr create --base main --title "Plan 9B: the SDK on minikube" --body $body
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust`, `msrv`, `images`, `chart`). Do not merge; the
controller merges after the final review.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §12 end to end: SDK clients through the full stack (register, verify, connect via invite, chat, receipts, edit, delete) | 1, 2 |
| §12 adverse 1 (connection dropped mid-send: stored once) — an app restart mid-send | 1, 2 |
| §12 adverse 6 (device revoked while connected) and the owner's 4003 → stop, through the SDK | 1, 2 |
| §5.4 a password reset signs out (4001) and the same device signs in again | 1, 2 |
| §7.4 4006 retried by the SDK without signing out | 1, 2 |
| §7.3 the frame budget: no 4004 for the SDK on a cluster | 2 |
| §8.4 the outbox survives restarts and keeps order | 1, 2 |
| §11 the smoke Pod: read-only root filesystem, data in an emptyDir, no secret in arguments, no token in logs | 1, 2 |

## Verified on minikube

Run on 2026-10-14 (profile `push`, Kubernetes 1.31), as an upgrade of the stopped profile
(revision 15) and again with `-SkipBuild` (revision 16): both `Phase: Succeeded`
(`PASS in 15143 ms` and `PASS in 15400 ms`); no token-like string or `Bearer` in the smoke
Pod's or the gateway's logs; eight `connection kicked` lines and no `too many frames` in
the gateway's log. The SDK's devices needed two or three reconnects each while the worker
relayed them (4006), and Erin's `SignedOut` came 2 s after the reset (the worker's relay).

## Deferred to later plans

- **The SDK scenario with groups on** (`gateway.groups=true`), once the owner offers
  groups and the SDK gains group calls.
- **A device that comes back after a long time offline** (a backlog of thousands of entries
  over several `Sync` batches) is covered by the SDK's unit tests (one `Ack` per batch);
  on a cluster it belongs with the load test of Plan 11.
- **wss and https** on a real certificate (Plan 11).
- **The bindings' own smoke tests** (Node and C apps doing the same scenario) with Plan 10.
- **The signup limit** caps smoke runs to about four an hour on one cluster (five signups a
  run against 20 an hour per IP).
