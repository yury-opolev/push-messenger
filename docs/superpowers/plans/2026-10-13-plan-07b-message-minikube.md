# Plan 7B — Message Service: minikube Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove Plan 7A end to end on the local minikube cluster with the same chart that will deploy to Azure. The smoke test keeps the real flow of Plan 6B (sign-up, the worker's emails and relays, the gateway's Send/Deliver/Ack) and adds everything 7A delivers: Alice and Bob become contacts by request and Carol by redeeming Alice's invite; Alice opens the direct conversation; auth-svc's `GET /v1/users?ids=…` shows each user only the profiles message-svc allows; after the chat Alice edits, Bob reads, Alice creates a group and deletes the message, and Bob's open WebSocket delivers every new entry while his history shows the message erased; finally Carol revokes her device and Bob resets his password through auth-svc, and the smoke test hears message-svc kick both devices on Redis once the worker relayed the events. The DEV ONLY `DevSetup` API, which those real calls replace, is removed. The chart generates auth-svc's Directory token into the kept Secret and gives it to auth-svc and message-svc only, tells auth-svc where message-svc is, and gives the smoke test the Redis URL from the Secret; `test-chart.ps1` has 29 checks.

**Architecture:** The smoke test calls message-svc's `Messaging` API straight, as the gateway will call it for an identified device (decision 1); its new modules `messaging.rs` (requests, edits, deletes, reads) and `kicks.rs` (a Redis subscription to `kick:{device_id}`) sit beside `auth.rs`, which learns to read profiles, revoke a device and reset a password through the worker's mailbox. The local stack of `tests/local.rs` wires the services as the chart does: auth-svc with `PUSH_MESSAGE_SVC_URL` and the Directory token, message-svc with both internal tokens. In the chart the Secret gets `directory-token` (48 random characters, generated once and kept with `lookup`), auth-svc's Deployment `PUSH_MESSAGE_SVC_URL` and `PUSH_DIRECTORY_TOKEN`, message-svc's `PUSH_DIRECTORY_TOKEN` in place of `PUSH_DEV_SETUP`, and the smoke Pod `--message-svc` and `PUSH_SMOKE_REDIS_URL` from the Secret's `redis-url`. No database changes: the existing volume and kept Secret upgrade in place. Spec: `docs/superpowers/specs/2026-10-11-push-messenger-design.md` §2, §5, §7.6, §11, §12; roadmap "Running locally on minikube".

**Tech Stack:** Docker (BuildKit), minikube 1.34 (Kubernetes 1.31, docker driver, ingress addon), kubectl, Helm 3.16, PowerShell 7 and bash. `push-smoke` now uses `redis` 1.2 and `prost` 0.14, both already in the workspace.

---

## Before you start

- **Plan 7A is merged.** This plan changes the smoke test, removes `DevSetup` from
  message-svc (and the tests that used it) and updates the chart.
- Work from the repository root: `C:\Users\yurio\Documents\github\push-messenger`. Every
  command below starts there.
- Create the branch from an up-to-date `main`:

```powershell
git switch main
git pull
git switch -c feature/plan-07b-message-minikube
```

- Fix a stale PATH for the current PowerShell session (each new shell needs it again):

```powershell
$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User')
```

- **Docker Desktop must be running**, with the resources of Plan 4B (the minikube node
  gets 4 CPUs and 6 GB). The minikube profile is `push` (Kubernetes 1.31). Note the
  current kubectl context first and restore it at the end — minikube switches it to `push`:

```powershell
kubectl config current-context
```

- **No wipe is needed for this plan's chart**: no database or init script changes, and the
  kept Secret gets `directory-token` on the upgrade. If `up` stops with "run down.ps1
  first" anyway (a volume from before Plan 6B, or one a restart left behind), run
  `deploy/minikube/down.ps1` once. Never delete the profile, and never touch other Docker
  containers on this machine.
- Helm may print `failed to load plugins … helm-diff … unknown field "platformHooks"`
  first: an outdated plugin, harmless.
- **Edit files with the edit tool, and keep them LF** (`.gitattributes`:
  `* text=auto eol=lf`). This matters for `up.sh`, which bash refuses with CRLF on Linux.
- **Replace blocks.** "Replace … with …" pairs below are exact copies of the file before and
  after the change, with two lines of context (more where two would not make the first block
  unique): find the first block in the file and replace it with the second. "Create" and
  "Replace the whole file" blocks are whole files.
- Baseline (after Plan 7A): `cargo test --workspace --locked` passes 808 tests;
  `deploy/helm/test-chart.ps1` passes 28 checks.
- **Every task ends green:**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
```

  plus the task's own checks (`test-chart.ps1`, minikube).
- **Scope rule:** no behaviour changes in `push-domain`, `push-store`, `push-auth`,
  `push-worker` or `push-gateway` (only the gateway's tests stop using `DevSetup`); no TLS,
  Key Vault, NetworkPolicy or registry pushes (Plan 11).

## Key rules the configuration must preserve

- **One token per internal API, given only to its caller.** `accounts-token` reaches
  message-svc and the worker; `directory-token` reaches message-svc and auth-svc; the
  gateway gets neither. Both are generated on install, kept on upgrades (`lookup`) and
  reach containers only through `secretKeyRef`.
- **No secret in git, in a ConfigMap or in a Pod's arguments.** The smoke test reads the
  Redis URL, which holds its password, from the Secret through its environment; the smoke
  test never prints it (`Target`'s `Debug` redacts it, Redis errors are reported by kind).
- **Each service owns its database.** auth-svc reaches message-svc only through
  `Directory`, never `push_messaging`.
- **DEV ONLY switches stay behind the dev-mode guard.** With `DevSetup` gone, dev mode
  turns on only the gateway's identities without a token and the worker's mailbox, and
  makes `helm test` the smoke test.
- **The smoke test never prints bodies, tokens or token-bearing URLs**: it reports message
  ids, seqs, codes and timings only, and message-svc's errors by their fixed texts.

## Decisions made for this plan

These were settled while writing the plan and verified on minikube. Plan 7A's decisions and
the owner's earlier decisions apply. Do not reopen any of this during execution.

1. **The smoke test calls message-svc's `Messaging` API straight for the new features**,
   as the gateway will call it for an identified device, rather than through new gateway
   frames. Plan 8 rewrites the gateway's frame handling (JWT checks, `ReAuth`, the request
   frames, close codes, kicks); frames added now would be built twice. The gRPC calls are
   exactly what the gateway will forward, and the gateway is still proven in the loop: the
   real Send/Deliver/Ack flow stays, and Bob's open WebSocket must deliver the edit, the
   group's first entry and the delete, which reach it only through message-svc's Redis
   notifications and the gateway's syncs.
2. **Kicks are checked on Redis.** The gateway closes kicked sockets with Plan 8; until
   then the smoke test subscribes to `kick:{device_id}` of Bob's and Carol's devices before
   anything can kick them, and waits (30 s at most, like every worker step) for
   `DEVICE_REVOKED` after Carol revokes her device with `DELETE /v1/devices/{id}`, and
   `PASSWORD_RESET` after Bob resets his password with the link the worker delivers. That
   proves the whole chain: auth-svc's outbox, the worker's relay, message-svc's commit and
   publish. It also checks that message-svc then refuses Carol's device.
3. **The smoke Pod gets the Redis URL from the Secret** (`PUSH_SMOKE_REDIS_URL`,
   `secretKeyRef: redis-url`), never as an argument. The smoke test is a DEV ONLY `helm
   test` Pod; it already reaches the worker's mailbox and message-svc inside the cluster.
4. **`DevSetup` is removed** (message-svc, its proto, `PUSH_DEV_SETUP`, the chart): its
   users and devices come from `Accounts` since Plan 6, its contacts and conversations from
   the real requests now. Tests that used it set up through `Accounts`, the request frames,
   or the domain directly.
5. **`--setup` becomes `--message-svc`** (`PUSH_SMOKE_MESSAGE_SVC_URL`) now that it no
   longer names a setup API; every target value may come from the environment, and the
   error names both the flag and the variable.
6. **Three users.** Carol is the invite's redeemer, the group's third member, the user Bob
   cannot see until they share the group, and the device that is revoked.
7. **No wipe, no new checks in the up scripts.** The chart adds no database and no init
   script; `directory-token` is generated into the kept Secret on the upgrade, like
   `accounts-token` in Plan 6B. Verified: a second `up.ps1 -SkipBuild -Smoke` keeps the
   token.
8. **`test-chart.ps1`**: the two DevSetup checks become "the DevSetup switch is gone" and
   "the smoke test calls message-svc and reads the Redis URL from the Secret", and one new
   check covers the Directory token: 29 checks.

## File structure

```
crates/push-smoke/Cargo.toml          + prost, redis
crates/push-smoke/src/lib.rs          the new flow; Target: --message-svc, --redis
crates/push-smoke/src/auth.rs         profiles, device revocation, password reset
crates/push-smoke/src/messaging.rs    NEW: message-svc's Messaging API, as the gateway calls it
crates/push-smoke/src/kicks.rs        NEW: kicks heard on Redis
crates/push-smoke/tests/local.rs      auth-svc wired to message-svc; no dev setup
crates/push-proto/proto/push/internal/v1/messaging.proto   - DevSetup
crates/push-message-svc/src/grpc.rs, config.rs, server.rs, lib.rs   - DevSetup
crates/push-message-svc/tests/service.rs   set up through Accounts and requests
crates/push-gateway/tests/gateway.rs  set up through the domain
deploy/helm/push-messenger/
├── values.yaml                       comment
└── templates/
    ├── _helpers.tpl, NOTES.txt       comments
    ├── secret.yaml                   + directory-token
    ├── auth.yaml                     PUSH_MESSAGE_SVC_URL, PUSH_DIRECTORY_TOKEN
    ├── message-svc.yaml              PUSH_DIRECTORY_TOKEN instead of PUSH_DEV_SETUP
    └── tests/smoke.yaml              --message-svc; PUSH_SMOKE_REDIS_URL from the Secret
deploy/helm/test-chart.ps1            29 checks
README.md, spec §5, §7.6, §11, roadmap   docs
```

---

### Task 1: The smoke test makes contacts, redeems an invite and reads profiles

**Files:**
- Create: `crates/push-smoke/src/messaging.rs`
- Modify: `crates/push-smoke/src/auth.rs`, `src/lib.rs`, `crates/push-smoke/tests/local.rs`

- [ ] **Step 1: The local stack as the chart wires it, failing**

auth-svc gets message-svc's URL and its Directory token, message-svc that token and no dev
setup, and the target names message-svc instead of a setup API (decision 5). In
`crates/push-smoke/tests/local.rs`:

Replace

```rust
//! The walking skeleton on this machine: Postgres and Redis in containers (Docker must be
//! running), auth-svc, the worker, message-svc and the gateway in this process, and the
//! smoke test against them over real HTTP, gRPC and WebSockets.

use std::net::SocketAddr;
```

with

```rust
//! The whole system on this machine: Postgres and Redis in containers (Docker must be
//! running), auth-svc, the worker, message-svc and the gateway in this process, wired as
//! the chart wires them (each internal API behind its own token), and the smoke test
//! against them over real HTTP, gRPC and WebSockets.

use std::net::SocketAddr;
```

Replace

```rust
    let auth_health_addr = auth_health.local_addr().unwrap();
    let auth_url = format!("http://{}", auth_http.local_addr().unwrap());
    let auth_variables = [
        ("PUSH_DATABASE_URL", auth_database_url.as_str()),
```

with

```rust
    let auth_health_addr = auth_health.local_addr().unwrap();
    let auth_url = format!("http://{}", auth_http.local_addr().unwrap());
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc_url = format!("http://{}", grpc.local_addr().unwrap());
    // auth-svc's token for message-svc's Directory API, as the chart's Secret holds it.
    let directory_token = "directory-token-for-the-local-smoke-test";
    let auth_variables = [
        ("PUSH_DATABASE_URL", auth_database_url.as_str()),
```

Replace

```rust
        ),
        ("PUSH_AUTH_WORKER_ROLE", "push_worker"),
    ];
    let auth_config = push_auth::Config::from_lookup(|name| {
```

with

```rust
        ),
        ("PUSH_AUTH_WORKER_ROLE", "push_worker"),
        ("PUSH_MESSAGE_SVC_URL", message_svc_url.as_str()),
        ("PUSH_DIRECTORY_TOKEN", directory_token),
    ];
    let auth_config = push_auth::Config::from_lookup(|name| {
```

Replace

```rust
    ));

    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc_health_addr = message_svc_health.local_addr().unwrap();
    let message_svc_url = format!("http://{}", grpc.local_addr().unwrap());
    // The worker's token for message-svc's Accounts API, as the chart's Secret holds it.
    let accounts_token = "accounts-token-for-the-local-smoke-test".to_owned();
```

with

```rust
    ));

    let message_svc_health = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let message_svc_health_addr = message_svc_health.local_addr().unwrap();
    // The worker's token for message-svc's Accounts API, as the chart's Secret holds it.
    let accounts_token = "accounts-token-for-the-local-smoke-test".to_owned();
```

Replace

```rust
            store: StoreOptions::default(),
            accounts_token: Some(accounts_token.clone()),
            directory_token: None,
            dev_setup: true,
        },
        grpc,
```

with

```rust
            store: StoreOptions::default(),
            accounts_token: Some(accounts_token.clone()),
            directory_token: Some(directory_token.to_owned()),
            dev_setup: false,
        },
        grpc,
```

Replace

```rust
        mailbox_url: worker_url,
        auth_url,
        setup_url: message_svc_url,
        socket_url,
        host: None,
```

with

```rust
        mailbox_url: worker_url,
        auth_url,
        message_svc_url,
        socket_url,
        host: None,
```

```powershell
cargo test -p push-smoke --test local
```

Expected: a compile error — `Target` has no field `message_svc_url`.

- [ ] **Step 2: message-svc's API as the gateway calls it, and profiles**

Create `crates/push-smoke/src/messaging.rs`:

```rust
//! The smoke test's calls to message-svc's `Messaging` API, made as the gateway makes them
//! for an identified device. Until the gateway carries these frames (Plan 8), the smoke
//! test calls message-svc straight.

use anyhow::{Context, anyhow, bail};
use push_proto::internal::v1::RequestCall;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::v1::{self, request, response};
use push_proto::{error_from_status, id_bytes};
use tonic::Status;
use tonic::transport::Channel;
use uuid::Uuid;

/// message-svc as the gateway calls it.
pub(crate) struct Messaging {
    client: MessagingClient<Channel>,
    next_request: u64,
}

impl Messaging {
    pub fn new(channel: Channel) -> Self {
        Self {
            client: MessagingClient::new(channel),
            next_request: 0,
        }
    }

    /// Sends a request frame as `user`; its answer must carry the request's id.
    pub async fn request(
        &mut self,
        user: Uuid,
        body: request::Body,
        what: &str,
    ) -> anyhow::Result<response::Body> {
        self.next_request += 1;
        let call = RequestCall {
            user_id: id_bytes(user),
            request: Some(v1::Request {
                request_id: self.next_request,
                body: Some(body),
            }),
        };
        let response = self
            .client
            .request(call)
            .await
            .map_err(|status| refused(what, &status))?
            .into_inner();
        if response.request_id != self.next_request {
            bail!("{what}: the answer carries another request's id");
        }
        response
            .body
            .with_context(|| format!("{what}: an empty answer"))
    }
}

/// A refusal, described by the client error message-svc sent: fixed texts, never a body.
fn refused(what: &str, status: &Status) -> anyhow::Error {
    let error = error_from_status(status);
    anyhow!("{what}: {:?} ({})", error.code(), error.message)
}
```

In `crates/push-smoke/src/auth.rs` (the access token kept for later steps, profiles, and
the mailbox read for any template):

Replace

```rust
//! worker's log sender delivered to the DEV ONLY mailbox, signs in, registers a device (and
//! retries the registration, which must find the same device) and refreshes its tokens, all
//! over auth-svc's public HTTP API, and checks the access token against the JWKS.

use std::time::{Duration, Instant};
```

with

```rust
//! worker's log sender delivered to the DEV ONLY mailbox, signs in, registers a device (and
//! retries the registration, which must find the same device) and refreshes its tokens, all
//! over auth-svc's public HTTP API, and checks the access token against the JWKS. Later the
//! users read each other's profiles there too.

use std::time::{Duration, Instant};
```

Replace

```rust
    pub user: Uuid,
    pub device: Uuid,
}
```

with

```rust
    pub user: Uuid,
    pub device: Uuid,
    /// The device's current access token. Never printed.
    pub access_token: String,
}
```

Replace

```rust
        .await?;

        let token = self.verification_token(name, &email).await?;
        let page = public(
            &self.http,
```

with

```rust
        .await?;

        let (token, waited) = self.mailbox_token(&email, "verify_email").await?;
        step(&format!(
            "{name} signed up; the worker's log sender delivered the verification email after {} ms",
            waited.as_millis()
        ));
        let page = public(
            &self.http,
```

Replace

```rust
            user,
            device: device_id,
        })
    }

    /// The token of the verification link sent to `email`: the worker relays the
    /// `email.send` event from auth-svc's outbox and its log sender drops the email into
    /// the dev mailbox, which is polled until it arrives.
    async fn verification_token(&self, name: &str, email: &str) -> anyhow::Result<String> {
        let url = format!(
            "{}/dev/mailbox",
```

with

```rust
            user,
            device: device_id,
            access_token: field(&refreshed, "access_token")?,
        })
    }

    /// The user ids among `ids` whose profiles `GET /v1/users?ids=…` shows the holder of
    /// `access_token`; every profile shown must have a handle.
    pub async fn profiles(&self, access_token: &str, ids: &[Uuid]) -> anyhow::Result<Vec<Uuid>> {
        let ids: Vec<String> = ids.iter().map(Uuid::to_string).collect();
        let answer = expect(
            public(&self.http, self.target, reqwest::Method::GET, "/v1/users")
                .query(&[("ids", ids.join(","))])
                .bearer_auth(access_token),
            StatusCode::OK,
            "read profiles",
        )
        .await?;
        let users = answer["users"]
            .as_array()
            .context("the profiles answer has no users")?;
        users
            .iter()
            .map(|user| {
                field(user, "handle")?;
                Ok(field(user, "user_id")?.parse()?)
            })
            .collect()
    }

    /// The token of the link in the newest `template` email to `email`, and how long it
    /// took to come: the worker relays the `email.send` event from auth-svc's outbox and
    /// its log sender drops the email into the dev mailbox, which is polled until it
    /// arrives.
    async fn mailbox_token(
        &self,
        email: &str,
        template: &str,
    ) -> anyhow::Result<(String, Duration)> {
        let url = format!(
            "{}/dev/mailbox",
```

Replace

```rust
                emails
                    .iter()
                    .find(|email| email["template"] == "verify_email")
                    .and_then(|email| email["text"].as_str().map(str::to_owned))
            });
```

with

```rust
                emails
                    .iter()
                    .rev()
                    .find(|email| email["template"] == template)
                    .and_then(|email| email["text"].as_str().map(str::to_owned))
            });
```

Replace

```rust
                }
                None => bail!(
                    "no verification email for {email} in the dev mailbox within {WORKER_TIMEOUT:?}"
                ),
            }
        };
        step(&format!(
            "{name} signed up; the worker's log sender delivered the verification email after {} ms",
            started.elapsed().as_millis()
        ));
        let link = text
            .lines()
            .find(|line| line.starts_with("http"))
            .context("the verification email has no link")?;
        let token = Url::parse(link)
            .map_err(|_| anyhow::anyhow!("the verification link is not a URL"))?
            .query_pairs()
            .find(|(key, _)| key == "token")
            .map(|(_, token)| token.into_owned())
            .context("the verification link has no token")?;
        Ok(token)
    }
```

with

```rust
                }
                None => bail!(
                    "no {template} email for {email} in the dev mailbox within {WORKER_TIMEOUT:?}"
                ),
            }
        };
        let link = text
            .lines()
            .find(|line| line.starts_with("http"))
            .with_context(|| format!("the {template} email has no link"))?;
        let token = Url::parse(link)
            .map_err(|_| anyhow::anyhow!("the {template} link is not a URL"))?
            .query_pairs()
            .find(|(key, _)| key == "token")
            .map(|(_, token)| token.into_owned())
            .with_context(|| format!("the {template} link has no token"))?;
        Ok((token, started.elapsed()))
    }
```

- [ ] **Step 3: Three users, contacts by request and invite, profiles**

Replace the whole of `crates/push-smoke/src/lib.rs` with:

```rust
//! push-smoke: the end-to-end smoke test of a running push-messenger.
//!
//! Against a deployment it:
//!
//! 1. enrolls Alice, Bob and Carol through auth-svc's public API: each signs up, waits for
//!    the verification email that the worker's log sender delivers to the DEV ONLY
//!    mailbox, verifies with its link, signs in, registers a device, checks the access
//!    token against the JWKS and refreshes it;
//! 2. waits until the worker has relayed the users and devices from auth-svc's outbox to
//!    message-svc (message-svc's `Identify` accepts each device);
//! 3. over message-svc's `Messaging` API, called as the gateway calls it for an identified
//!    device (the gateway carries these frames from Plan 8): Alice asks Bob to be a
//!    contact and Bob accepts, Carol redeems Alice's invite, Alice opens the direct
//!    conversation with Bob; auth-svc shows Alice the profiles of Bob and Carol, and Bob
//!    none of Carol's;
//! 4. connects Bob's WebSocket and then Alice's, each with a DEV ONLY `Identify` naming
//!    the device auth-svc registered, and waits for `Ready`;
//! 5. sends a message from Alice and waits for her `SendAck` and Bob's `Deliver`; acks it
//!    from Bob, then pings: the `Pong` proves the ack went through, since the gateway
//!    handles a connection's frames in order and answers a failed ack with an `Error`.
//!
//! The deployment must run the worker with the log sender and `PUSH_DEV_MAILBOX=1`, and
//! the gateway with `PUSH_DEV_AUTH=1`.

mod auth;
mod messaging;

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use push_proto::internal::v1::IdentifyRequest;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::v1::frame::Body;
use push_proto::v1::{
    Ack, Contact, ContactAccept, ContactRequest, ContactState, ContentType, CreateInvite, Deliver,
    Identify, InviteRedeemed, OpenDirect, Ping, RedeemInvite, Send, SendAck, request, response,
};
use push_proto::{decode_frame, encode_frame, id_bytes};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tonic::transport::Endpoint;
use uuid::Uuid;

use crate::messaging::Messaging;
/// How long each step may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to keep trying to connect: an ingress may answer 503 for a few seconds after
/// its backends become ready.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the worker may take to deliver an email or relay an event: it looks for work
/// every second.
pub(crate) const WORKER_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a closing handshake may take; the test has passed by then.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Where the deployment is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Base URL of auth-svc's public API: `http://host` (the ingress controller).
    pub auth_url: String,
    /// Base URL of the worker, for the DEV ONLY mailbox of its log sender, which the
    /// ingress does not route: `http://push-worker:8080`.
    pub mailbox_url: String,
    /// gRPC URL of message-svc, to see the relayed devices and to call its `Messaging` API
    /// as the gateway does: `http://host:50051`.
    pub message_svc_url: String,
    /// WebSocket URL of the gateway: `ws://host/v1/socket`.
    pub socket_url: String,
    /// `Host` header for the WebSocket and public API requests, when the URLs name an
    /// ingress controller rather than the virtual host it routes (`push.local`).
    pub host: Option<String>,
}

/// The usage line of the `push-smoke` binary.
pub const USAGE: &str = "usage: push-smoke --auth <url> --mailbox <url> --message-svc <grpc-url> --socket <ws-url> [--host <host>]";

impl Target {
    /// Reads `--auth`, `--mailbox`, `--message-svc`, `--socket` and `--host` from `args`;
    /// each one missing there is read from `env` (`PUSH_SMOKE_AUTH_URL`,
    /// `PUSH_SMOKE_MAILBOX_URL`, `PUSH_SMOKE_MESSAGE_SVC_URL`, `PUSH_SMOKE_SOCKET_URL`,
    /// `PUSH_SMOKE_HOST`).
    pub fn parse(
        args: impl IntoIterator<Item = String>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let (mut auth_url, mut mailbox_url, mut message_svc_url) = (None, None, None);
        let (mut socket_url, mut host) = (None, None);
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let slot = match flag.as_str() {
                "--auth" => &mut auth_url,
                "--mailbox" => &mut mailbox_url,
                "--message-svc" => &mut message_svc_url,
                "--socket" => &mut socket_url,
                "--host" => &mut host,
                other => return Err(format!("unknown argument {other:?}; {USAGE}")),
            };
            *slot = Some(args.next().ok_or_else(|| format!("{flag} needs a value"))?);
        }
        let required = |value: Option<String>, flag: &str, variable: &str| {
            value
                .or_else(|| env(variable))
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("{flag} or {variable} is required; {USAGE}"))
        };
        Ok(Self {
            auth_url: required(auth_url, "--auth", "PUSH_SMOKE_AUTH_URL")?,
            mailbox_url: required(mailbox_url, "--mailbox", "PUSH_SMOKE_MAILBOX_URL")?,
            message_svc_url: required(
                message_svc_url,
                "--message-svc",
                "PUSH_SMOKE_MESSAGE_SVC_URL",
            )?,
            socket_url: required(socket_url, "--socket", "PUSH_SMOKE_SOCKET_URL")?,
            host: host
                .or_else(|| env("PUSH_SMOKE_HOST"))
                .filter(|host| !host.is_empty()),
        })
    }
}
/// One user with one device.
struct Client {
    name: &'static str,
    user: Uuid,
    device: Uuid,
    /// Never printed.
    access_token: String,
}

/// Everyone in the smoke test.
struct People {
    alice: Client,
    bob: Client,
    carol: Client,
}

/// Runs the smoke test, printing each step. Fails at the first step that does not
/// complete within 10 s (30 s for what waits for the worker).
pub async fn run(target: &Target) -> anyhow::Result<()> {
    let started = Instant::now();
    let auth = auth::Auth::connect(target, STEP_TIMEOUT).await?;
    let alice = auth.enroll("alice").await?;
    let bob = auth.enroll("bob").await?;
    let carol = auth.enroll("carol").await?;

    let channel = Endpoint::from_shared(target.message_svc_url.clone())
        .with_context(|| format!("message-svc URL {}", target.message_svc_url))?
        .connect_timeout(STEP_TIMEOUT)
        .timeout(STEP_TIMEOUT)
        .connect()
        .await
        .with_context(|| format!("connect to message-svc at {}", target.message_svc_url))?;
    let people = People {
        alice: Client::relayed("alice", alice, &channel).await?,
        bob: Client::relayed("bob", bob, &channel).await?,
        carol: Client::relayed("carol", carol, &channel).await?,
    };
    let mut api = Messaging::new(channel);
    let conversation = connect(&mut api, &auth, &people).await?;

    let mut bob_socket = people.bob.connect(target).await?;
    let mut alice_socket = people.alice.connect(target).await?;
    chat(
        &mut alice_socket,
        &mut bob_socket,
        &people.alice,
        &conversation,
    )
    .await?;
    for socket in [&mut alice_socket, &mut bob_socket] {
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, socket.close(None)).await;
    }
    step(&format!("PASS in {} ms", started.elapsed().as_millis()));
    Ok(())
}

/// Contacts by request and by invite, the direct conversation, and the profiles each may
/// see. Returns the direct conversation of Alice and Bob.
async fn connect(
    api: &mut Messaging,
    auth: &auth::Auth<'_>,
    people: &People,
) -> anyhow::Result<Vec<u8>> {
    let People { alice, bob, carol } = people;
    let ask = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(bob.user),
    });
    let asked = api
        .request(alice.user, ask, "alice's contact request")
        .await?;
    let pending = Contact {
        user_id: id_bytes(bob.user),
        state: ContactState::RequestedOut.into(),
    };
    if asked != response::Body::Contact(pending) {
        bail!("alice's contact request answered {asked:?}");
    }
    let accept = request::Body::ContactAccept(ContactAccept {
        user_id: id_bytes(alice.user),
    });
    api.request(bob.user, accept, "bob's accept").await?;
    step("alice asked bob to be a contact and bob accepted");

    let create = request::Body::CreateInvite(CreateInvite {
        expires_in_s: 600,
        max_uses: 1,
    });
    let response::Body::Invite(invite) = api.request(alice.user, create, "alice's invite").await?
    else {
        bail!("alice's invite was answered with something else");
    };
    let redeem = request::Body::RedeemInvite(RedeemInvite { code: invite.code });
    let redeemed = api.request(carol.user, redeem, "carol's redeem").await?;
    let creator = InviteRedeemed {
        user_id: id_bytes(alice.user),
    };
    if redeemed != response::Body::InviteRedeemed(creator) {
        bail!("carol's redeem answered {redeemed:?}");
    }
    step("carol redeemed alice's invite: they are contacts");

    let open = request::Body::OpenDirect(OpenDirect {
        peer_user_id: id_bytes(bob.user),
    });
    let response::Body::DirectOpened(opened) =
        api.request(alice.user, open, "open the chat").await?
    else {
        bail!("opening the chat was answered with something else");
    };
    step("alice opened the direct conversation with bob");

    let mut seen = auth
        .profiles(&alice.access_token, &[bob.user, carol.user])
        .await?;
    seen.sort();
    let mut contacts = vec![bob.user, carol.user];
    contacts.sort();
    if seen != contacts {
        bail!("auth-svc showed alice the profiles of {seen:?}, not bob's and carol's");
    }
    if !auth
        .profiles(&bob.access_token, &[carol.user])
        .await?
        .is_empty()
    {
        bail!("auth-svc showed bob carol's profile, though they share nothing");
    }
    step(
        "auth-svc shows alice the profiles of her contacts bob and carol, and bob none of carol's",
    );
    Ok(opened.conversation_id)
}

/// Alice sends a message over the gateway and Bob receives and acks it.
async fn chat(
    alice_socket: &mut Socket,
    bob_socket: &mut Socket,
    alice: &Client,
    conversation: &[u8],
) -> anyhow::Result<SendAck> {
    let client_msg_id = id_bytes(Uuid::now_v7());
    let text = format!("hello from push-smoke {}", Uuid::now_v7());
    let sent_at = Instant::now();
    send(
        alice_socket,
        Body::Send(Send {
            client_msg_id: client_msg_id.clone(),
            conversation_id: conversation.to_vec(),
            content_type: ContentType::Text.into(),
            body: text.clone().into_bytes(),
            client_ts: None,
        }),
    )
    .await?;
    let ack = loop {
        match next(alice_socket, "alice's SendAck").await? {
            Body::SendAck(ack) if ack.client_msg_id == client_msg_id => break ack,
            Body::Error(error) => bail!("alice's send failed: {error:?}"),
            // Alice's own devices also receive her message.
            _ => {}
        }
    };
    step(&format!(
        "alice got SendAck seq={} after {} ms",
        ack.seq,
        sent_at.elapsed().as_millis()
    ));

    let entry = delivered(bob_socket, conversation, ack.seq, "bob's Deliver").await?;
    if entry.body.as_deref() != Some(text.as_bytes()) || entry.sender_id != id_bytes(alice.user) {
        bail!("bob received the wrong message: {entry:?}");
    }
    step(&format!(
        "bob got Deliver seq={} after {} ms",
        entry.seq,
        sent_at.elapsed().as_millis()
    ));

    send(
        bob_socket,
        Body::Ack(Ack {
            conversation_id: conversation.to_vec(),
            seq: entry.seq,
        }),
    )
    .await?;
    send(bob_socket, Body::Ping(Ping {})).await?;
    loop {
        match next(bob_socket, "bob's Pong").await? {
            Body::Pong(_) => break,
            Body::Error(error) => bail!("bob's ack failed: {error:?}"),
            _ => {}
        }
    }
    step("bob acked; the gateway answered the following Ping");
    Ok(ack)
}

/// Waits for the `Deliver` of `seq` in `conversation` on `socket`.
async fn delivered(
    socket: &mut Socket,
    conversation: &[u8],
    seq: i64,
    waiting_for: &str,
) -> anyhow::Result<Deliver> {
    loop {
        match next(socket, waiting_for).await? {
            Body::Deliver(entry) if entry.seq == seq && entry.conversation_id == conversation => {
                return Ok(entry);
            }
            Body::Error(error) => bail!("an error while waiting for {waiting_for}: {error:?}"),
            _ => {}
        }
    }
}
impl Client {
    /// Waits until the worker has relayed the user and device auth-svc enrolled to
    /// message-svc, which then accepts the device in `Identify`.
    async fn relayed(
        name: &'static str,
        enrolled: auth::Enrolled,
        channel: &tonic::transport::Channel,
    ) -> anyhow::Result<Self> {
        let (user, device) = (enrolled.user, enrolled.device);
        let mut messaging = MessagingClient::new(channel.clone());
        let started = Instant::now();
        let mut reported = None;
        loop {
            let identified = messaging
                .identify(IdentifyRequest {
                    user_id: id_bytes(user),
                    device_id: id_bytes(device),
                })
                .await;
            match identified {
                Ok(_) => break,
                // Revoked, or another user's: waiting cannot help.
                Err(status) if status.code() == tonic::Code::PermissionDenied => bail!(
                    "message-svc refused {name}'s device {device} as revoked or another user's"
                ),
                // UNAVAILABLE is message-svc's answer for a device not relayed yet.
                Err(status) if started.elapsed() < WORKER_TIMEOUT => {
                    // Other codes (message-svc restarting, say) are reported once each.
                    if status.code() != tonic::Code::Unavailable && reported != Some(status.code())
                    {
                        reported = Some(status.code());
                        step(&format!(
                            "message-svc answers {:?} yet; retrying",
                            status.code()
                        ));
                    }
                }
                Err(status) => bail!(
                    "the worker did not relay {name}'s device to message-svc within {WORKER_TIMEOUT:?}: {:?}",
                    status.code()
                ),
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        step(&format!(
            "the worker relayed {name}'s account and device {device} to message-svc (waited {} ms)",
            started.elapsed().as_millis()
        ));
        Ok(Self {
            name,
            user,
            device,
            access_token: enrolled.access_token,
        })
    }

    /// Connects and identifies; returns once `Ready` arrived.
    async fn connect(&self, target: &Target) -> anyhow::Result<Socket> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut socket = loop {
            let mut request = target.socket_url.as_str().into_client_request()?;
            if let Some(host) = &target.host {
                request
                    .headers_mut()
                    .insert("Host", HeaderValue::from_str(host)?);
            }
            match tokio::time::timeout(STEP_TIMEOUT, connect_async(request)).await {
                Ok(Ok((socket, _))) => break socket,
                Ok(Err(error)) if Instant::now() < deadline => {
                    step(&format!(
                        "{} cannot connect yet ({error}); retrying",
                        self.name
                    ));
                }
                Ok(Err(error)) => {
                    return Err(error)
                        .context(format!("connect {} to {}", self.name, target.socket_url));
                }
                Err(_) => bail!("connecting {} timed out", self.name),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        send(
            &mut socket,
            Body::Identify(Identify {
                token: String::new(),
                dev_user_id: id_bytes(self.user),
                dev_device_id: id_bytes(self.device),
            }),
        )
        .await?;
        match next(&mut socket, "Ready").await? {
            Body::Ready(ready) => {
                step(&format!(
                    "{} connected (Ready, heartbeat {} ms)",
                    self.name, ready.heartbeat_ms
                ));
                Ok(socket)
            }
            other => bail!("{} expected Ready, got {other:?}", self.name),
        }
    }
}

async fn send(socket: &mut Socket, body: Body) -> anyhow::Result<()> {
    socket
        .send(Message::Binary(encode_frame(body).into()))
        .await
        .context("send a frame")
}

/// The next frame, within the step timeout.
async fn next(socket: &mut Socket, waiting_for: &str) -> anyhow::Result<Body> {
    loop {
        let message = tokio::time::timeout(STEP_TIMEOUT, socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("no {waiting_for} within {STEP_TIMEOUT:?}"))?
            .with_context(|| format!("the connection ended while waiting for {waiting_for}"))?
            .with_context(|| format!("the connection failed while waiting for {waiting_for}"))?;
        match message {
            Message::Binary(bytes) => return decode_frame(&bytes).context("decode a frame"),
            Message::Close(frame) => bail!("closed while waiting for {waiting_for}: {frame:?}"),
            _ => {}
        }
    }
}

fn step(message: &str) {
    println!("smoke: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn flags_name_the_target() {
        let target = Target::parse(
            args(&[
                "--auth",
                "http://ingress",
                "--mailbox",
                "http://worker:8080",
                "--message-svc",
                "http://svc:50051",
                "--socket",
                "ws://ingress/v1/socket",
                "--host",
                "push.local",
            ]),
            |_| None,
        );
        assert_eq!(
            target,
            Ok(Target {
                auth_url: "http://ingress".to_owned(),
                mailbox_url: "http://worker:8080".to_owned(),
                message_svc_url: "http://svc:50051".to_owned(),
                socket_url: "ws://ingress/v1/socket".to_owned(),
                host: Some("push.local".to_owned()),
            })
        );
    }

    #[test]
    fn missing_flags_come_from_the_environment() {
        let target = Target::parse(
            args(&["--socket", "ws://gw/v1/socket"]),
            |name| match name {
                "PUSH_SMOKE_AUTH_URL" => Some("http://auth:8080".to_owned()),
                "PUSH_SMOKE_MAILBOX_URL" => Some("http://worker:8080".to_owned()),
                "PUSH_SMOKE_MESSAGE_SVC_URL" => Some("http://svc:50051".to_owned()),
                _ => None,
            },
        )
        .unwrap();
        assert_eq!(target.message_svc_url, "http://svc:50051");
        assert_eq!(target.host, None);
    }

    #[test]
    fn a_missing_url_or_unknown_flag_is_refused() {
        let all = |name: &str| Some(format!("value of {name}"));
        assert!(Target::parse(args(&[]), all).is_ok());
        for missing in [
            "PUSH_SMOKE_AUTH_URL",
            "PUSH_SMOKE_MAILBOX_URL",
            "PUSH_SMOKE_MESSAGE_SVC_URL",
            "PUSH_SMOKE_SOCKET_URL",
        ] {
            let error =
                Target::parse(args(&[]), |name| all(name).filter(|_| name != missing)).unwrap_err();
            assert!(error.contains(missing), "{error}");
        }
        assert!(Target::parse(args(&["--verbose"]), all).is_err());
        assert!(Target::parse(args(&["--message-svc"]), all).is_err());
    }
}
```

```powershell
cargo test -p push-smoke
cargo test -p push-smoke --test local -- --nocapture
```

Expected: the unit tests pass (`flags_name_the_target`,
`missing_flags_come_from_the_environment`, `a_missing_url_or_unknown_flag_is_refused` and
the 3 of `auth.rs`), and the local run prints, with other ids and timings:

```
smoke: alice signed up; the worker's log sender delivered the verification email after 527 ms
smoke: alice verified the email, signed in and registered device 01a137a7-9de9-7123-847a-c48413e64eda (a retry returned the same device) in 636 ms
smoke: alice's access token verifies against the JWKS (EdDSA, aud ws://127.0.0.1:54606/v1/socket); refresh rotated it
smoke: bob signed up; the worker's log sender delivered the verification email after 254 ms
smoke: bob verified the email, signed in and registered device 01a137a7-9f51-7419-beee-cb53d2b8d785 (a retry returned the same device) in 321 ms
smoke: bob's access token verifies against the JWKS (EdDSA, aud ws://127.0.0.1:54606/v1/socket); refresh rotated it
smoke: carol signed up; the worker's log sender delivered the verification email after 261 ms
smoke: carol verified the email, signed in and registered device 01a137a7-a0a2-7703-8acb-324db0fa3ea9 (a retry returned the same device) in 331 ms
smoke: carol's access token verifies against the JWKS (EdDSA, aud ws://127.0.0.1:54606/v1/socket); refresh rotated it
smoke: the worker relayed alice's account and device 01a137a7-9de9-7123-847a-c48413e64eda to message-svc (waited 2 ms)
smoke: the worker relayed bob's account and device 01a137a7-9f51-7419-beee-cb53d2b8d785 to message-svc (waited 1 ms)
smoke: the worker relayed carol's account and device 01a137a7-a0a2-7703-8acb-324db0fa3ea9 to message-svc (waited 280 ms)
smoke: alice asked bob to be a contact and bob accepted
smoke: carol redeemed alice's invite: they are contacts
smoke: alice opened the direct conversation with bob
smoke: auth-svc shows alice the profiles of her contacts bob and carol, and bob none of carol's
smoke: bob connected (Ready, heartbeat 30000 ms)
smoke: alice connected (Ready, heartbeat 30000 ms)
smoke: alice got SendAck seq=1 after 27 ms
smoke: bob got Deliver seq=1 after 35 ms
smoke: bob acked; the gateway answered the following Ping
smoke: PASS in 1924 ms
```

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "test(smoke): contacts, an invite and profiles through message-svc and auth-svc"
```

Expected: 808 tests pass (the smoke test's unit tests were rewritten, not added), no
warnings, no diff.

---

### Task 2: The smoke test edits, reads, forms a group, deletes and hears kicks

**Files:**
- Create: `crates/push-smoke/src/kicks.rs`
- Modify: `crates/push-smoke/Cargo.toml`, `src/messaging.rs`, `src/auth.rs`, `src/lib.rs`,
  `crates/push-smoke/tests/local.rs`, `Cargo.lock`

- [ ] **Step 1: The target names Redis, failing**

The smoke test hears kicks on Redis (decision 2), so the local target passes the Redis
URL. In `crates/push-smoke/tests/local.rs`:

Replace

```rust
//! running), auth-svc, the worker, message-svc and the gateway in this process, wired as
//! the chart wires them (each internal API behind its own token), and the smoke test
//! against them over real HTTP, gRPC and WebSockets.

use std::net::SocketAddr;
```

with

```rust
//! running), auth-svc, the worker, message-svc and the gateway in this process, wired as
//! the chart wires them (each internal API behind its own token), and the smoke test
//! against them over real HTTP, gRPC, WebSockets and Redis.

use std::net::SocketAddr;
```

Replace

```rust
        redis.get_host_port_ipv4(6379).await.unwrap()
    );
    let shutdown = CancellationToken::new();
```

with

```rust
        redis.get_host_port_ipv4(6379).await.unwrap()
    );
    let redis_url_for_smoke = redis_url.clone();
    let shutdown = CancellationToken::new();
```

Replace

```rust
        push_gateway::Config {
            message_svc_url: message_svc_url.clone(),
            redis_url,
            listen_addr: listener.local_addr().unwrap(),
            health_addr: gateway_health.local_addr().unwrap(),
```

with

```rust
        push_gateway::Config {
            message_svc_url: message_svc_url.clone(),
            redis_url: redis_url.clone(),
            listen_addr: listener.local_addr().unwrap(),
            health_addr: gateway_health.local_addr().unwrap(),
```

Replace

```rust
        auth_url,
        message_svc_url,
        socket_url,
        host: None,
```

with

```rust
        auth_url,
        message_svc_url,
        redis_url: redis_url_for_smoke,
        socket_url,
        host: None,
```

```powershell
cargo test -p push-smoke --test local
```

Expected: a compile error — `Target` has no field `redis_url`.

- [ ] **Step 2: Edits, deletes, reads, kicks, a revocation and a password reset**

In `crates/push-smoke/Cargo.toml`:

Replace

```toml
futures-util.workspace = true
jsonwebtoken.workspace = true
push-proto.workspace = true
reqwest.workspace = true
serde.workspace = true
```

with

```toml
futures-util.workspace = true
jsonwebtoken.workspace = true
prost.workspace = true
push-proto.workspace = true
redis.workspace = true
reqwest.workspace = true
serde.workspace = true
```

In `crates/push-smoke/src/messaging.rs`:

Replace

```rust
//! The smoke test's calls to message-svc's `Messaging` API, made as the gateway makes them
//! for an identified device. Until the gateway carries these frames (Plan 8), the smoke
//! test calls message-svc straight.

use anyhow::{Context, anyhow, bail};
use push_proto::internal::v1::RequestCall;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::v1::{self, request, response};
use push_proto::{error_from_status, id_bytes};
use tonic::Status;
```

with

```rust
//! The smoke test's calls to message-svc's `Messaging` API, made as the gateway makes them
//! for an identified device: the request frames, Edit, Delete and ReadUpTo. Until the
//! gateway carries these frames (Plan 8), the smoke test calls message-svc straight.

use anyhow::{Context, anyhow, bail};
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{DeleteRequest, EditRequest, ReadUpToRequest, RequestCall};
use push_proto::v1::{self, Delete, Edit, ReadUpTo, SendAck, delete, edit, request, response};
use push_proto::{error_from_status, id_bytes};
use tonic::Status;
```

Replace

```rust
            .with_context(|| format!("{what}: an empty answer"))
    }
}
```

with

```rust
            .with_context(|| format!("{what}: an empty answer"))
    }

    /// Replaces the text of `user`'s message `target` in `conversation`.
    pub async fn edit(
        &mut self,
        user: Uuid,
        device: Uuid,
        conversation: &[u8],
        target: &[u8],
        body: &str,
    ) -> anyhow::Result<SendAck> {
        let request = EditRequest {
            user_id: id_bytes(user),
            device_id: id_bytes(device),
            edit: Some(Edit {
                client_msg_id: id_bytes(Uuid::now_v7()),
                conversation_id: conversation.to_vec(),
                target: Some(edit::Target::TargetMessageId(target.to_vec())),
                body: body.as_bytes().to_vec(),
                client_ts: None,
            }),
        };
        let ack = self.client.edit(request).await;
        Ok(ack.map_err(|status| refused("edit", &status))?.into_inner())
    }

    /// Deletes `user`'s message that was sent with `client_msg_id`.
    pub async fn delete(
        &mut self,
        user: Uuid,
        device: Uuid,
        conversation: &[u8],
        client_msg_id: &[u8],
    ) -> anyhow::Result<SendAck> {
        let request = DeleteRequest {
            user_id: id_bytes(user),
            device_id: id_bytes(device),
            delete: Some(Delete {
                client_msg_id: id_bytes(Uuid::now_v7()),
                conversation_id: conversation.to_vec(),
                target: Some(delete::Target::TargetClientMsgId(client_msg_id.to_vec())),
            }),
        };
        let ack = self.client.delete(request).await;
        Ok(ack
            .map_err(|status| refused("delete", &status))?
            .into_inner())
    }

    /// Moves `user`'s read position; whether it moved.
    pub async fn read_up_to(
        &mut self,
        user: Uuid,
        conversation: &[u8],
        seq: i64,
    ) -> anyhow::Result<bool> {
        let request = ReadUpToRequest {
            user_id: id_bytes(user),
            read: Some(ReadUpTo {
                conversation_id: conversation.to_vec(),
                seq,
            }),
        };
        let moved = self.client.read_up_to(request).await;
        Ok(moved
            .map_err(|status| refused("read", &status))?
            .into_inner()
            .moved)
    }
}
```

Create `crates/push-smoke/src/kicks.rs`:

```rust
//! Kicks, as the gateway will receive them (Plan 8 closes the sockets): the smoke test
//! listens on the devices' Redis channels while auth-svc revokes a device and resets a
//! password, and the worker relays both to message-svc.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use futures_util::StreamExt;
use prost::Message;
use push_proto::internal::v1::{Kick, KickReason};
use push_proto::kick_channel;
use redis::aio::PubSub;
use uuid::Uuid;

use crate::WORKER_TIMEOUT;

/// A subscription to the kick channels of some devices.
pub(crate) struct Kicks {
    pubsub: PubSub,
}

impl Kicks {
    /// Subscribes to `kick:{device_id}` of each device. Errors never quote `url`, which
    /// holds the Redis password.
    pub async fn subscribe(url: &str, devices: &[Uuid]) -> anyhow::Result<Self> {
        let unreachable = |error: redis::RedisError| anyhow!("Redis: {:?}", error.kind());
        let client = redis::Client::open(url).map_err(unreachable)?;
        let mut pubsub = client.get_async_pubsub().await.map_err(unreachable)?;
        for device in devices {
            pubsub
                .subscribe(kick_channel(*device))
                .await
                .map_err(unreachable)?;
        }
        Ok(Self { pubsub })
    }

    /// Waits for `device` to be kicked for `reason`; how long that took.
    pub async fn kicked(&mut self, device: Uuid, reason: KickReason) -> anyhow::Result<Duration> {
        let started = Instant::now();
        let channel = kick_channel(device);
        let mut messages = self.pubsub.on_message();
        loop {
            let left = WORKER_TIMEOUT.saturating_sub(started.elapsed());
            let Ok(message) = tokio::time::timeout(left, messages.next()).await else {
                bail!("{device} was not kicked within {WORKER_TIMEOUT:?}");
            };
            let message = message.ok_or_else(|| anyhow!("the Redis subscription ended"))?;
            if message.get_channel_name() != channel {
                continue;
            }
            let kick = Kick::decode(message.get_payload_bytes())?;
            if kick.reason() != reason {
                bail!(
                    "{device} was kicked for {:?}, not {reason:?}",
                    kick.reason()
                );
            }
            return Ok(started.elapsed());
        }
    }
}
```

In `crates/push-smoke/src/auth.rs` (the email kept for the reset, a revocation and a
password reset through the link the worker delivers):

Replace

```rust
//! retries the registration, which must find the same device) and refreshes its tokens, all
//! over auth-svc's public HTTP API, and checks the access token against the JWKS. Later the
//! users read each other's profiles there too.

use std::time::{Duration, Instant};
```

with

```rust
//! retries the registration, which must find the same device) and refreshes its tokens, all
//! over auth-svc's public HTTP API, and checks the access token against the JWKS. Later the
//! users read each other's profiles, revoke a device and reset a password there too.

use std::time::{Duration, Instant};
```

Replace

```rust
    pub user: Uuid,
    pub device: Uuid,
    /// The device's current access token. Never printed.
    pub access_token: String,
```

with

```rust
    pub user: Uuid,
    pub device: Uuid,
    pub email: String,
    /// The device's current access token. Never printed.
    pub access_token: String,
```

Replace

```rust
            user,
            device: device_id,
            access_token: field(&refreshed, "access_token")?,
        })
```

with

```rust
            user,
            device: device_id,
            email,
            access_token: field(&refreshed, "access_token")?,
        })
```

Replace

```rust
    }

    /// The token of the link in the newest `template` email to `email`, and how long it
    /// took to come: the worker relays the `email.send` event from auth-svc's outbox and
```

with

```rust
    }

    /// Revokes `device` with its own access token, as a user signing a device out does.
    pub async fn revoke_device(&self, access_token: &str, device: Uuid) -> anyhow::Result<()> {
        let path = format!("/v1/devices/{device}");
        let response = public(&self.http, self.target, reqwest::Method::DELETE, &path)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|error| error.without_url())
            .context("revoke a device: request failed")?;
        if response.status() != StatusCode::NO_CONTENT {
            bail!("revoke a device: answered {}", response.status());
        }
        Ok(())
    }

    /// Resets the password of `email` through the link the worker delivers.
    pub async fn reset_password(&self, email: &str) -> anyhow::Result<()> {
        expect(
            self.post("/v1/auth/password/forgot", json!({"email": email})),
            StatusCode::ACCEPTED,
            "ask for a password reset",
        )
        .await?;
        let (token, _) = self.mailbox_token(email, "reset_password").await?;
        let password = format!("smoke-{}", Uuid::now_v7());
        expect(
            self.post(
                "/v1/auth/password/reset",
                json!({"token": token, "password": password}),
            ),
            StatusCode::OK,
            "reset the password",
        )
        .await?;
        Ok(())
    }

    /// The token of the link in the newest `template` email to `email`, and how long it
    /// took to come: the worker relays the `email.send` event from auth-svc's outbox and
```

- [ ] **Step 3: The rest of the flow**

In `crates/push-smoke/src/lib.rs` (the Redis URL from `--redis` or `PUSH_SMOKE_REDIS_URL`,
never shown; the subscription before anything can kick; the changes Bob's socket must
deliver; the kicks):

Replace

```rust
//! 5. sends a message from Alice and waits for her `SendAck` and Bob's `Deliver`; acks it
//!    from Bob, then pings: the `Pong` proves the ack went through, since the gateway
//!    handles a connection's frames in order and answers a failed ack with an `Error`.
//!
//! The deployment must run the worker with the log sender and `PUSH_DEV_MAILBOX=1`, and
```

with

```rust
//! 5. sends a message from Alice and waits for her `SendAck` and Bob's `Deliver`; acks it
//!    from Bob, then pings: the `Pong` proves the ack went through, since the gateway
//!    handles a connection's frames in order and answers a failed ack with an `Error`;
//! 6. Alice edits the message and Bob's socket delivers the edit; Bob reads up to the
//!    message and Alice's listing shows it read; Alice creates a group with Bob and Carol,
//!    Bob's socket delivers its first entry and auth-svc now shows Bob Carol's profile;
//!    Alice deletes the message, Bob's socket delivers the delete, and Bob's history holds
//!    the message erased;
//! 7. Carol revokes her device and Bob resets his password through auth-svc; the worker
//!    relays both, message-svc kicks Carol's device (revoked) and Bob's (password reset) on
//!    their Redis channels, where the smoke test listens, and refuses Carol's device.
//!
//! The deployment must run the worker with the log sender and `PUSH_DEV_MAILBOX=1`, and
```

Replace

```rust
mod auth;
mod messaging;
```

with

```rust
mod auth;
mod kicks;
mod messaging;
```

Replace

```rust
use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use push_proto::internal::v1::IdentifyRequest;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::v1::frame::Body;
use push_proto::v1::{
    Ack, Contact, ContactAccept, ContactRequest, ContactState, ContentType, CreateInvite, Deliver,
    Identify, InviteRedeemed, OpenDirect, Ping, RedeemInvite, Send, SendAck, request, response,
};
use push_proto::{decode_frame, encode_frame, id_bytes};
```

with

```rust
use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{IdentifyRequest, KickReason};
use push_proto::v1::frame::Body;
use push_proto::v1::{
    Ack, Contact, ContactAccept, ContactRequest, ContactState, ContentType, CreateGroup,
    CreateInvite, Deliver, EntryKind, FetchHistory, Identify, InviteRedeemed, ListConversations,
    OpenDirect, Ping, RedeemInvite, Send, SendAck, conversation_summary, request, response,
};
use push_proto::{decode_frame, encode_frame, id_bytes};
```

Replace

```rust
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tonic::transport::Endpoint;
use uuid::Uuid;

use crate::messaging::Messaging;
/// How long each step may take.
```

with

```rust
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

use crate::kicks::Kicks;
use crate::messaging::Messaging;
/// How long each step may take.
```

Replace

```rust
/// Where the deployment is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Base URL of auth-svc's public API: `http://host` (the ingress controller).
```

with

```rust
/// Where the deployment is.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    /// Base URL of auth-svc's public API: `http://host` (the ingress controller).
```

Replace

```rust
    /// as the gateway does: `http://host:50051`.
    pub message_svc_url: String,
    /// WebSocket URL of the gateway: `ws://host/v1/socket`.
    pub socket_url: String,
```

with

```rust
    /// as the gateway does: `http://host:50051`.
    pub message_svc_url: String,
    /// Redis URL, to hear the kicks message-svc publishes: `redis://:password@host:6379`.
    /// It holds a password, so it is never shown.
    pub redis_url: String,
    /// WebSocket URL of the gateway: `ws://host/v1/socket`.
    pub socket_url: String,
```

Replace

```rust
}

/// The usage line of the `push-smoke` binary.
pub const USAGE: &str = "usage: push-smoke --auth <url> --mailbox <url> --message-svc <grpc-url> --socket <ws-url> [--host <host>]";

impl Target {
    /// Reads `--auth`, `--mailbox`, `--message-svc`, `--socket` and `--host` from `args`;
    /// each one missing there is read from `env` (`PUSH_SMOKE_AUTH_URL`,
    /// `PUSH_SMOKE_MAILBOX_URL`, `PUSH_SMOKE_MESSAGE_SVC_URL`, `PUSH_SMOKE_SOCKET_URL`,
    /// `PUSH_SMOKE_HOST`).
    pub fn parse(
        args: impl IntoIterator<Item = String>,
```

with

```rust
}

impl std::fmt::Debug for Target {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Target")
            .field("auth_url", &self.auth_url)
            .field("mailbox_url", &self.mailbox_url)
            .field("message_svc_url", &self.message_svc_url)
            .field("redis_url", &"<redacted>")
            .field("socket_url", &self.socket_url)
            .field("host", &self.host)
            .finish()
    }
}

/// The usage line of the `push-smoke` binary.
pub const USAGE: &str = "usage: push-smoke --auth <url> --mailbox <url> --message-svc <grpc-url> --socket <ws-url> [--host <host>], with the Redis URL in PUSH_SMOKE_REDIS_URL (or --redis <url>)";

impl Target {
    /// Reads `--auth`, `--mailbox`, `--message-svc`, `--redis`, `--socket` and `--host`
    /// from `args`; each one missing there is read from `env` (`PUSH_SMOKE_AUTH_URL`,
    /// `PUSH_SMOKE_MAILBOX_URL`, `PUSH_SMOKE_MESSAGE_SVC_URL`, `PUSH_SMOKE_REDIS_URL`,
    /// `PUSH_SMOKE_SOCKET_URL`, `PUSH_SMOKE_HOST`). The Redis URL holds a password, so a
    /// deployment passes it in the environment, from its Secret.
    pub fn parse(
        args: impl IntoIterator<Item = String>,
```

Replace

```rust
    ) -> Result<Self, String> {
        let (mut auth_url, mut mailbox_url, mut message_svc_url) = (None, None, None);
        let (mut socket_url, mut host) = (None, None);
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
```

with

```rust
    ) -> Result<Self, String> {
        let (mut auth_url, mut mailbox_url, mut message_svc_url) = (None, None, None);
        let (mut redis_url, mut socket_url, mut host) = (None, None, None);
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
```

Replace

```rust
                "--mailbox" => &mut mailbox_url,
                "--message-svc" => &mut message_svc_url,
                "--socket" => &mut socket_url,
                "--host" => &mut host,
```

with

```rust
                "--mailbox" => &mut mailbox_url,
                "--message-svc" => &mut message_svc_url,
                "--redis" => &mut redis_url,
                "--socket" => &mut socket_url,
                "--host" => &mut host,
```

Replace

```rust
                "PUSH_SMOKE_MESSAGE_SVC_URL",
            )?,
            socket_url: required(socket_url, "--socket", "PUSH_SMOKE_SOCKET_URL")?,
            host: host
```

with

```rust
                "PUSH_SMOKE_MESSAGE_SVC_URL",
            )?,
            redis_url: required(redis_url, "--redis", "PUSH_SMOKE_REDIS_URL")?,
            socket_url: required(socket_url, "--socket", "PUSH_SMOKE_SOCKET_URL")?,
            host: host
```

Replace

```rust
    user: Uuid,
    device: Uuid,
    /// Never printed.
    access_token: String,
```

with

```rust
    user: Uuid,
    device: Uuid,
    email: String,
    /// Never printed.
    access_token: String,
```

Replace

```rust
        carol: Client::relayed("carol", carol, &channel).await?,
    };
    let mut api = Messaging::new(channel);
    let conversation = connect(&mut api, &auth, &people).await?;

    let mut bob_socket = people.bob.connect(target).await?;
    let mut alice_socket = people.alice.connect(target).await?;
    chat(
        &mut alice_socket,
        &mut bob_socket,
```

with

```rust
        carol: Client::relayed("carol", carol, &channel).await?,
    };
    // Listening before anything can kick them.
    let mut kicks =
        Kicks::subscribe(&target.redis_url, &[people.bob.device, people.carol.device]).await?;
    let mut api = Messaging::new(channel.clone());
    let conversation = connect(&mut api, &auth, &people).await?;

    let mut bob_socket = people.bob.connect(target).await?;
    let mut alice_socket = people.alice.connect(target).await?;
    let (sent, client_msg_id) = chat(
        &mut alice_socket,
        &mut bob_socket,
```

Replace

```rust
    )
    .await?;
    for socket in [&mut alice_socket, &mut bob_socket] {
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, socket.close(None)).await;
    }
    step(&format!("PASS in {} ms", started.elapsed().as_millis()));
    Ok(())
```

with

```rust
    )
    .await?;
    let change = Change {
        conversation: &conversation,
        sent: &sent,
        client_msg_id: &client_msg_id,
    };
    change_messages(&mut api, &auth, &mut bob_socket, &people, &change).await?;
    for socket in [&mut alice_socket, &mut bob_socket] {
        let _ = tokio::time::timeout(CLOSE_TIMEOUT, socket.close(None)).await;
    }

    kick(&mut kicks, &auth, &channel, &people).await?;
    step(&format!("PASS in {} ms", started.elapsed().as_millis()));
    Ok(())
```

Replace

```rust
}

/// Alice sends a message over the gateway and Bob receives and acks it.
async fn chat(
    alice_socket: &mut Socket,
```

with

```rust
}

/// Alice sends a message over the gateway and Bob receives and acks it: Alice's
/// `SendAck` and the message's `client_msg_id`.
async fn chat(
    alice_socket: &mut Socket,
```

Replace

```rust
    alice: &Client,
    conversation: &[u8],
) -> anyhow::Result<SendAck> {
    let client_msg_id = id_bytes(Uuid::now_v7());
    let text = format!("hello from push-smoke {}", Uuid::now_v7());
```

with

```rust
    alice: &Client,
    conversation: &[u8],
) -> anyhow::Result<(SendAck, Vec<u8>)> {
    let client_msg_id = id_bytes(Uuid::now_v7());
    let text = format!("hello from push-smoke {}", Uuid::now_v7());
```

Replace

```rust
    }
    step("bob acked; the gateway answered the following Ping");
    Ok(ack)
}
```

with

```rust
    }
    step("bob acked; the gateway answered the following Ping");
    Ok((ack, client_msg_id))
}

/// The message Alice sent in her direct conversation with Bob.
struct Change<'a> {
    conversation: &'a [u8],
    sent: &'a SendAck,
    client_msg_id: &'a [u8],
}

/// Alice edits the message, Bob reads it, Alice creates a group and deletes the message;
/// Bob's socket delivers every entry they add.
async fn change_messages(
    api: &mut Messaging,
    auth: &auth::Auth<'_>,
    bob_socket: &mut Socket,
    people: &People,
    message: &Change<'_>,
) -> anyhow::Result<()> {
    let People { alice, bob, carol } = people;
    let Change {
        conversation,
        sent,
        client_msg_id,
    } = *message;
    let edited = api
        .edit(
            alice.user,
            alice.device,
            conversation,
            &sent.message_id,
            "hello, edited",
        )
        .await?;
    let entry = delivered(bob_socket, conversation, edited.seq, "the edit").await?;
    if entry.kind() != EntryKind::Edit || entry.target_seq != Some(sent.seq) {
        bail!("bob's socket delivered {entry:?} instead of the edit");
    }
    step(&format!(
        "alice edited her message; bob's socket delivered the edit (seq {})",
        edited.seq
    ));

    if !api.read_up_to(bob.user, conversation, sent.seq).await? {
        bail!("bob's read position did not move");
    }
    let list = request::Body::ListConversations(ListConversations::default());
    let response::Body::Conversations(listed) =
        api.request(alice.user, list, "alice's listing").await?
    else {
        bail!("alice's listing was answered with something else");
    };
    let read = listed
        .conversations
        .iter()
        .find(|summary| summary.conversation_id == conversation)
        .and_then(|summary| match &summary.receipts {
            Some(conversation_summary::Receipts::Direct(receipts)) => Some(receipts.peer_read_seq),
            _ => None,
        });
    if read != Some(sent.seq) {
        bail!(
            "alice's listing shows bob read up to {read:?}, not {}",
            sent.seq
        );
    }
    step("bob read the message; alice's listing shows it read");

    let create = request::Body::CreateGroup(CreateGroup {
        title: "Smoke".to_owned(),
        member_ids: vec![id_bytes(bob.user), id_bytes(carol.user)],
    });
    let response::Body::GroupCreated(group) =
        api.request(alice.user, create, "create a group").await?
    else {
        bail!("creating a group was answered with something else");
    };
    let entry = delivered(bob_socket, &group.conversation_id, 1, "the group").await?;
    if entry.kind() != EntryKind::System {
        bail!("the group's first entry is {entry:?}, not a system entry");
    }
    if auth.profiles(&bob.access_token, &[carol.user]).await? != [carol.user] {
        bail!("auth-svc does not show bob carol's profile, though they share a group");
    }
    step(
        "alice created a group with bob and carol; bob's socket delivered its first entry and auth-svc now shows bob carol's profile",
    );

    let deleted = api
        .delete(alice.user, alice.device, conversation, client_msg_id)
        .await?;
    let entry = delivered(bob_socket, conversation, deleted.seq, "the delete").await?;
    if entry.kind() != EntryKind::Delete {
        bail!("bob's socket delivered {entry:?} instead of the delete");
    }
    let fetch = request::Body::FetchHistory(FetchHistory {
        conversation_id: conversation.to_vec(),
        before_seq: None,
        limit: 0,
    });
    let response::Body::History(page) = api.request(bob.user, fetch, "bob's history").await? else {
        bail!("bob's history was answered with something else");
    };
    match page.entries.as_slice() {
        [message] if message.seq == sent.seq && message.deleted && message.body.is_none() => {}
        entries => bail!(
            "bob's history is not the erased message alone ({} entries)",
            entries.len()
        ),
    }
    step(&format!(
        "alice deleted her message; bob's socket delivered the delete (seq {}) and his history holds it erased",
        deleted.seq
    ));
    Ok(())
}

/// Carol revokes her device and Bob resets his password; message-svc kicks both devices
/// once the worker relays the events, and refuses Carol's device from then on.
async fn kick(
    kicks: &mut Kicks,
    auth: &auth::Auth<'_>,
    channel: &Channel,
    people: &People,
) -> anyhow::Result<()> {
    let People { bob, carol, .. } = people;
    auth.revoke_device(&carol.access_token, carol.device)
        .await?;
    let waited = kicks
        .kicked(carol.device, KickReason::DeviceRevoked)
        .await?;
    let identified = MessagingClient::new(channel.clone())
        .identify(IdentifyRequest {
            user_id: id_bytes(carol.user),
            device_id: id_bytes(carol.device),
        })
        .await;
    match identified {
        Err(status) if status.code() == tonic::Code::PermissionDenied => {}
        Err(status) => bail!(
            "message-svc answered {:?} for carol's revoked device",
            status.code()
        ),
        Ok(_) => bail!("message-svc still accepts carol's revoked device"),
    }
    step(&format!(
        "carol revoked her device; the worker relayed it and message-svc kicked it on Redis after {} ms, and refuses it now",
        waited.as_millis()
    ));

    auth.reset_password(&bob.email).await?;
    let waited = kicks.kicked(bob.device, KickReason::PasswordReset).await?;
    step(&format!(
        "bob reset his password; the worker relayed it and message-svc kicked his device on Redis after {} ms",
        waited.as_millis()
    ));
    Ok(())
}
```

Replace

```rust
            user,
            device,
            access_token: enrolled.access_token,
        })
```

with

```rust
            user,
            device,
            email: enrolled.email,
            access_token: enrolled.access_token,
        })
```

Replace

```rust
                "--message-svc",
                "http://svc:50051",
                "--socket",
                "ws://ingress/v1/socket",
```

with

```rust
                "--message-svc",
                "http://svc:50051",
                "--redis",
                "redis://:secret@redis:6379",
                "--socket",
                "ws://ingress/v1/socket",
```

Replace

```rust
                mailbox_url: "http://worker:8080".to_owned(),
                message_svc_url: "http://svc:50051".to_owned(),
                socket_url: "ws://ingress/v1/socket".to_owned(),
                host: Some("push.local".to_owned()),
```

with

```rust
                mailbox_url: "http://worker:8080".to_owned(),
                message_svc_url: "http://svc:50051".to_owned(),
                redis_url: "redis://:secret@redis:6379".to_owned(),
                socket_url: "ws://ingress/v1/socket".to_owned(),
                host: Some("push.local".to_owned()),
```

Replace

```rust
                "PUSH_SMOKE_MAILBOX_URL" => Some("http://worker:8080".to_owned()),
                "PUSH_SMOKE_MESSAGE_SVC_URL" => Some("http://svc:50051".to_owned()),
                _ => None,
            },
```

with

```rust
                "PUSH_SMOKE_MAILBOX_URL" => Some("http://worker:8080".to_owned()),
                "PUSH_SMOKE_MESSAGE_SVC_URL" => Some("http://svc:50051".to_owned()),
                "PUSH_SMOKE_REDIS_URL" => Some("redis://:secret@redis:6379".to_owned()),
                _ => None,
            },
```

Replace

```rust
        .unwrap();
        assert_eq!(target.message_svc_url, "http://svc:50051");
        assert_eq!(target.host, None);
    }
```

with

```rust
        .unwrap();
        assert_eq!(target.message_svc_url, "http://svc:50051");
        assert_eq!(target.redis_url, "redis://:secret@redis:6379");
        assert_eq!(target.host, None);
        assert!(!format!("{target:?}").contains("secret"));
    }
```

Replace

```rust
            "PUSH_SMOKE_MAILBOX_URL",
            "PUSH_SMOKE_MESSAGE_SVC_URL",
            "PUSH_SMOKE_SOCKET_URL",
        ] {
```

with

```rust
            "PUSH_SMOKE_MAILBOX_URL",
            "PUSH_SMOKE_MESSAGE_SVC_URL",
            "PUSH_SMOKE_REDIS_URL",
            "PUSH_SMOKE_SOCKET_URL",
        ] {
```

```powershell
cargo test -p push-smoke
cargo test -p push-smoke --test local -- --nocapture
```

Expected: cargo records `prost` and `redis` for `push-smoke` in `Cargo.lock`; the unit
tests pass, and the local run prints, with other ids and timings:

```
smoke: alice signed up; the worker's log sender delivered the verification email after 259 ms
smoke: alice verified the email, signed in and registered device 01a13787-bd6e-7257-92f1-a90ce5db5017 (a retry returned the same device) in 366 ms
smoke: alice's access token verifies against the JWKS (EdDSA, aud ws://127.0.0.1:62847/v1/socket); refresh rotated it
smoke: bob signed up; the worker's log sender delivered the verification email after 253 ms
smoke: bob verified the email, signed in and registered device 01a13787-bec4-74cd-8151-838ce925ec8b (a retry returned the same device) in 321 ms
smoke: bob's access token verifies against the JWKS (EdDSA, aud ws://127.0.0.1:62847/v1/socket); refresh rotated it
smoke: carol signed up; the worker's log sender delivered the verification email after 265 ms
smoke: carol verified the email, signed in and registered device 01a13787-c033-7163-b928-f0cb2df0e562 (a retry returned the same device) in 362 ms
smoke: carol's access token verifies against the JWKS (EdDSA, aud ws://127.0.0.1:62847/v1/socket); refresh rotated it
smoke: the worker relayed alice's account and device 01a13787-bd6e-7257-92f1-a90ce5db5017 to message-svc (waited 2 ms)
smoke: the worker relayed bob's account and device 01a13787-bec4-74cd-8151-838ce925ec8b to message-svc (waited 2 ms)
smoke: the worker relayed carol's account and device 01a13787-c033-7163-b928-f0cb2df0e562 to message-svc (waited 537 ms)
smoke: alice asked bob to be a contact and bob accepted
smoke: carol redeemed alice's invite: they are contacts
smoke: alice opened the direct conversation with bob
smoke: auth-svc shows alice the profiles of her contacts bob and carol, and bob none of carol's
smoke: bob connected (Ready, heartbeat 30000 ms)
smoke: alice connected (Ready, heartbeat 30000 ms)
smoke: alice got SendAck seq=1 after 27 ms
smoke: bob got Deliver seq=1 after 33 ms
smoke: bob acked; the gateway answered the following Ping
smoke: alice edited her message; bob's socket delivered the edit (seq 2)
smoke: bob read the message; alice's listing shows it read
smoke: alice created a group with bob and carol; bob's socket delivered its first entry and auth-svc now shows bob carol's profile
smoke: alice deleted her message; bob's socket delivered the delete (seq 3) and his history holds it erased
smoke: carol revoked her device; the worker relayed it and message-svc kicked it on Redis after 212 ms, and refuses it now
smoke: bob reset his password; the worker relayed it and message-svc kicked his device on Redis after 109 ms
smoke: PASS in 2678 ms
```

- [ ] **Step 4: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add Cargo.lock crates
git commit -m "test(smoke): edits, reads, a group, a delete and kicks end to end"
```

Expected: 808 tests pass, no warnings, no diff.

---

### Task 3: The DEV ONLY `DevSetup` API goes

**Files:**
- Modify: `crates/push-proto/proto/push/internal/v1/messaging.proto`,
  `crates/push-message-svc/src/grpc.rs`, `src/config.rs`, `src/server.rs`, `src/lib.rs`,
  `crates/push-message-svc/tests/service.rs`, `crates/push-gateway/tests/gateway.rs`,
  `crates/push-smoke/tests/local.rs`

- [ ] **Step 1: The tests set up without it**

message-svc's unit tests relay their users through `Accounts` and make contacts and chats
with request frames. In `crates/push-message-svc/src/grpc.rs`, together with the removal of
`DevSetupApi` itself and its imports:

Replace

```rust
//! The gRPC services: `Messaging` for the gateway, `Accounts` for the worker and the
//! DEV ONLY `DevSetup`.

use std::sync::Arc;

use push_domain::{
    ClientMsgId, Clock, ContactState, ConversationId, DeviceId, MAX_SYNC_BATCH, Messaging,
    Notifier, SendText, Store, UserId,
};
use push_proto::id_bytes;
use push_proto::internal::v1::{
    AccountDevice, AccountUser, AccountsResponse, AckRequest, AckResponse, DeleteRequest,
    EditRequest, IdentifyRequest, IdentifyResponse, MakeContactsRequest, MakeContactsResponse,
    OpenDirectRequest, OpenDirectResponse, ReadUpToRequest, ReadUpToResponse,
    RegisterDeviceRequest, RegisterDeviceResponse, RegisterUserRequest, RegisterUserResponse,
    RequestCall, SendRequest, SyncRequest, SyncResponse, VisibleUsersRequest, VisibleUsersResponse,
    accounts_server, dev_setup_server, directory_server, messaging_server,
};
use push_proto::v1::{ContentType, SendAck};
```

with

```rust
//! The gRPC services: `Messaging` for the gateway, `Accounts` for the worker and
//! `Directory` for auth-svc.

use std::sync::Arc;

use push_domain::{
    ClientMsgId, Clock, ConversationId, DeviceId, MAX_SYNC_BATCH, Messaging, Notifier, SendText,
    Store, UserId,
};
use push_proto::id_bytes;
use push_proto::internal::v1::{
    AccountDevice, AccountUser, AccountsResponse, AckRequest, AckResponse, DeleteRequest,
    EditRequest, IdentifyRequest, IdentifyResponse, ReadUpToRequest, ReadUpToResponse, RequestCall,
    SendRequest, SyncRequest, SyncResponse, VisibleUsersRequest, VisibleUsersResponse,
    accounts_server, directory_server, messaging_server,
};
use push_proto::v1::{ContentType, SendAck};
```

Replace

```rust
}

/// DEV ONLY: sets up users, devices, contacts and direct conversations without auth-svc.
/// `serve` registers it only when the configuration enables it (`PUSH_DEV_SETUP=1`).
pub struct DevSetupApi<S, C, N> {
    messaging: Arc<Messaging<S, C, N>>,
}

impl<S, C, N> DevSetupApi<S, C, N> {
    pub fn new(messaging: Arc<Messaging<S, C, N>>) -> Self {
        Self { messaging }
    }
}

#[tonic::async_trait]
impl<S, C, N> dev_setup_server::DevSetup for DevSetupApi<S, C, N>
where
    S: Store + 'static,
    C: Clock + 'static,
    N: Notifier + 'static,
{
    async fn register_user(
        &self,
        request: Request<RegisterUserRequest>,
    ) -> Result<Response<RegisterUserResponse>, Status> {
        let user = parse_id(&request.into_inner().user_id, "user_id", UserId)?;
        self.messaging
            .register_user(user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(RegisterUserResponse {}))
    }

    async fn register_device(
        &self,
        request: Request<RegisterDeviceRequest>,
    ) -> Result<Response<RegisterDeviceResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let device = parse_id(&request.device_id, "device_id", DeviceId)?;
        self.messaging
            .register_device(device, user)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(RegisterDeviceResponse {}))
    }

    async fn make_contacts(
        &self,
        request: Request<MakeContactsRequest>,
    ) -> Result<Response<MakeContactsResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let peer = parse_id(&request.peer_id, "peer_id", UserId)?;
        let state = self
            .messaging
            .contact_request(user, peer)
            .await
            .map_err(domain_error)?;
        if state != ContactState::Accepted {
            self.messaging
                .contact_accept(peer, user)
                .await
                .map_err(domain_error)?;
        }
        Ok(Response::new(MakeContactsResponse {}))
    }

    async fn open_direct(
        &self,
        request: Request<OpenDirectRequest>,
    ) -> Result<Response<OpenDirectResponse>, Status> {
        let request = request.into_inner();
        let user = parse_id(&request.user_id, "user_id", UserId)?;
        let peer = parse_id(&request.peer_id, "peer_id", UserId)?;
        let conversation = self
            .messaging
            .open_direct(user, peer)
            .await
            .map_err(domain_error)?;
        Ok(Response::new(OpenDirectResponse {
            conversation_id: id_bytes(conversation.0),
        }))
    }
}

#[cfg(test)]
mod tests {
    use push_domain::{Event, InMemoryStore, KickReason, RecordingNotifier, SystemClock};
    use push_proto::internal::v1::accounts_server::Accounts as AccountsRpc;
    use push_proto::internal::v1::dev_setup_server::DevSetup;
    use push_proto::internal::v1::directory_server::Directory as DirectoryRpc;
    use push_proto::internal::v1::messaging_server::Messaging as MessagingRpc;
    use push_proto::v1::{Ack, ErrorCode, Send};
    use push_proto::{error_from_status, id};
    use tonic::Code;
```

with

```rust
}

#[cfg(test)]
mod tests {
    use push_domain::{Event, InMemoryStore, KickReason, RecordingNotifier, SystemClock};
    use push_proto::internal::v1::accounts_server::Accounts as AccountsRpc;
    use push_proto::internal::v1::directory_server::Directory as DirectoryRpc;
    use push_proto::internal::v1::messaging_server::Messaging as MessagingRpc;
    use push_proto::v1::{
        Ack, ContactAccept, ContactRequest, ErrorCode, OpenDirect, Send, request, response,
    };
    use push_proto::{error_from_status, id};
    use tonic::Code;
```

Replace

```rust
    type Api = MessagingApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Accounts = AccountsApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Setup = DevSetupApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Directory = DirectoryApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
```

with

```rust
    type Api = MessagingApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Accounts = AccountsApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
    type Directory = DirectoryApi<InMemoryStore, SystemClock, Arc<RecordingNotifier>>;
```

Replace

```rust
        api: Api,
        accounts: Accounts,
        setup: Setup,
        directory: Directory,
        notifier: Arc<RecordingNotifier>,
```

with

```rust
        api: Api,
        accounts: Accounts,
        directory: Directory,
        notifier: Arc<RecordingNotifier>,
```

Replace

```rust
            api: MessagingApi::new(Arc::clone(&messaging)),
            accounts: AccountsApi::new(Arc::clone(&messaging)),
            setup: DevSetupApi::new(Arc::clone(&messaging)),
            directory: DirectoryApi::new(messaging),
            notifier,
```

with

```rust
            api: MessagingApi::new(Arc::clone(&messaging)),
            accounts: AccountsApi::new(Arc::clone(&messaging)),
            directory: DirectoryApi::new(messaging),
            notifier,
```

Replace

```rust
    }

    async fn person(world: &World) -> Person {
        let person = Person {
```

with

```rust
    }

    /// A user with one device, relayed as the worker relays them.
    async fn person(world: &World) -> Person {
        let person = Person {
```

Replace

```rust
        };
        world
            .setup
            .register_user(Request::new(RegisterUserRequest {
                user_id: id_bytes(person.user.0),
            }))
            .await
            .unwrap();
        world
            .setup
            .register_device(Request::new(RegisterDeviceRequest {
                user_id: id_bytes(person.user.0),
                device_id: id_bytes(person.device.0),
            }))
            .await
            .unwrap();
```

with

```rust
        };
        world
            .accounts
            .device_registered(account_device(person.user, person.device))
            .await
            .unwrap();
```

Replace

```rust
    }

    /// Alice and Bob as contacts with a direct conversation.
    async fn chat(world: &World) -> (Person, Person, ConversationId) {
        let (alice, bob) = (person(world).await, person(world).await);
        world
            .setup
            .make_contacts(Request::new(MakeContactsRequest {
                user_id: id_bytes(alice.user.0),
                peer_id: id_bytes(bob.user.0),
            }))
            .await
            .unwrap();
        let opened = world
            .setup
            .open_direct(Request::new(OpenDirectRequest {
                user_id: id_bytes(alice.user.0),
                peer_id: id_bytes(bob.user.0),
            }))
            .await
            .unwrap()
            .into_inner();
        let conversation = ConversationId(id(&opened.conversation_id).unwrap());
        (alice, bob, conversation)
```

with

```rust
    }

    /// Alice and Bob as contacts with a direct conversation, through request frames.
    async fn chat(world: &World) -> (Person, Person, ConversationId) {
        let (alice, bob) = (person(world).await, person(world).await);
        let call = |from: &Person, body: request::Body| {
            Request::new(RequestCall {
                user_id: id_bytes(from.user.0),
                request: Some(push_proto::v1::Request {
                    request_id: 1,
                    body: Some(body),
                }),
            })
        };
        let ask = request::Body::ContactRequest(ContactRequest {
            user_id: id_bytes(bob.user.0),
        });
        world.api.request(call(&alice, ask)).await.unwrap();
        let accept = request::Body::ContactAccept(ContactAccept {
            user_id: id_bytes(alice.user.0),
        });
        world.api.request(call(&bob, accept)).await.unwrap();
        let open = request::Body::OpenDirect(OpenDirect {
            peer_user_id: id_bytes(bob.user.0),
        });
        let opened = world.api.request(call(&alice, open)).await.unwrap();
        let Some(response::Body::DirectOpened(opened)) = opened.into_inner().body else {
            panic!("not an opened conversation");
        };
        let conversation = ConversationId(id(&opened.conversation_id).unwrap());
        (alice, bob, conversation)
```

Over real Postgres the same, through the worker's `Accounts` client; the test that checked
that `DevSetup` is absent keeps its `Accounts` checks under a new name. In
`crates/push-message-svc/tests/service.rs`:

Replace

```rust
//! The whole service on real Postgres and Redis (Docker must be running): readiness, the
//! dev setup calls, the worker's account events (a revocation kicks the device on Redis),
//! a send that rings the recipient's Redis channel, sync, ack, and graceful shutdown.

use std::net::SocketAddr;
```

with

```rust
//! The whole service on real Postgres and Redis (Docker must be running): readiness, the
//! worker's account events (a revocation kicks the device on Redis), a send that rings the
//! recipient's Redis channel, sync, ack, the client's requests, auth-svc's Directory, and
//! graceful shutdown.

use std::net::SocketAddr;
```

Replace

```rust
use push_message_svc::{Config, serve};
use push_proto::internal::v1::accounts_client::AccountsClient;
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::directory_client::DirectoryClient;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AccountDevice, AckRequest, EditRequest, IdentifyRequest, Kick, KickReason, MakeContactsRequest,
    Notification, OpenDirectRequest, ReadUpToRequest, RegisterDeviceRequest, RegisterUserRequest,
    RequestCall, SendRequest, SyncRequest, VisibleUsersRequest, notification::Kind,
};
use push_proto::v1::{
    self, Ack, ContactAccept, ContactRequest, ContentType, CreateGroup, CreateInvite, Edit,
    FetchHistory, GroupReceipts, ListConversations, ReadUpTo, RedeemInvite, Send,
    conversation_summary, edit, request, response,
};
```

with

```rust
use push_message_svc::{Config, serve};
use push_proto::internal::v1::accounts_client::AccountsClient;
use push_proto::internal::v1::directory_client::DirectoryClient;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::{
    AccountDevice, AckRequest, EditRequest, IdentifyRequest, Kick, KickReason, Notification,
    ReadUpToRequest, RequestCall, SendRequest, SyncRequest, VisibleUsersRequest,
    notification::Kind,
};
use push_proto::v1::{
    self, Ack, ContactAccept, ContactRequest, ContentType, CreateGroup, CreateInvite, Edit,
    FetchHistory, GroupReceipts, ListConversations, OpenDirect, ReadUpTo, RedeemInvite, Send,
    conversation_summary, edit, request, response,
};
```

Replace

```rust
/// Starts Postgres, Redis and the service on free local ports.
async fn start(dev_setup: bool) -> Running {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let postgres = Postgres::default()
```

with

```rust
/// Starts Postgres, Redis and the service on free local ports.
async fn start() -> Running {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let postgres = Postgres::default()
```

Replace

```rust
        accounts_token: Some(ACCOUNTS_TOKEN.to_owned()),
        directory_token: Some(DIRECTORY_TOKEN.to_owned()),
        dev_setup,
    };
    let shutdown = CancellationToken::new();
```

with

```rust
        accounts_token: Some(ACCOUNTS_TOKEN.to_owned()),
        directory_token: Some(DIRECTORY_TOKEN.to_owned()),
    };
    let shutdown = CancellationToken::new();
```

Replace

```rust
        accounts_token: None,
        directory_token: None,
        dev_setup: false,
    };
    let shutdown = CancellationToken::new();
```

with

```rust
        accounts_token: None,
        directory_token: None,
    };
    let shutdown = CancellationToken::new();
```

Replace

```rust
#[tokio::test]
async fn a_message_goes_from_send_to_ack_and_rings_the_recipient() {
    let running = start(true).await;
    assert_eq!(
        get(running.health, "/healthz").await,
```

with

```rust
#[tokio::test]
async fn a_message_goes_from_send_to_ack_and_rings_the_recipient() {
    let running = start().await;
    assert_eq!(
        get(running.health, "/healthz").await,
```

Replace

```rust
    let url = format!("http://{}", running.grpc);
    let mut setup = DevSetupClient::connect(url.clone()).await.unwrap();
    let mut messaging = MessagingClient::connect(url).await.unwrap();
    let (alice, bob) = (Uuid::now_v7(), Uuid::now_v7());
    let (alice_device, bob_device) = (Uuid::now_v7(), Uuid::now_v7());
    for (user, device) in [(alice, alice_device), (bob, bob_device)] {
        setup
            .register_user(RegisterUserRequest {
                user_id: id_bytes(user),
            })
            .await
            .unwrap();
        setup
            .register_device(RegisterDeviceRequest {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
```

with

```rust
    let url = format!("http://{}", running.grpc);
    let mut accounts = accounts_client(url.clone()).await;
    let mut messaging = MessagingClient::connect(url).await.unwrap();
    let (alice, bob) = (Uuid::now_v7(), Uuid::now_v7());
    let (alice_device, bob_device) = (Uuid::now_v7(), Uuid::now_v7());
    for (user, device) in [(alice, alice_device), (bob, bob_device)] {
        accounts
            .device_registered(AccountDevice {
                user_id: id_bytes(user),
                device_id: id_bytes(device),
```

Replace

```rust
            .unwrap();
    }
    setup
        .make_contacts(MakeContactsRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap();
    let conversation = setup
        .open_direct(OpenDirectRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap()
        .into_inner()
        .conversation_id;

    let mut bell = redis::Client::open(running.redis_url.as_str())
```

with

```rust
            .unwrap();
    }
    let ask = request::Body::ContactRequest(ContactRequest {
        user_id: id_bytes(bob),
    });
    messaging.request(call(alice, ask)).await.unwrap();
    let accept = request::Body::ContactAccept(ContactAccept {
        user_id: id_bytes(alice),
    });
    messaging.request(call(bob, accept)).await.unwrap();
    let open = request::Body::OpenDirect(OpenDirect {
        peer_user_id: id_bytes(bob),
    });
    let opened = messaging.request(call(alice, open)).await.unwrap();
    let Some(response::Body::DirectOpened(opened)) = opened.into_inner().body else {
        panic!("not an opened conversation");
    };
    let conversation = opened.conversation_id;

    let mut bell = redis::Client::open(running.redis_url.as_str())
```

Replace

```rust
#[tokio::test]
async fn accounts_are_served_and_dev_setup_is_absent_unless_enabled() {
    let running = start(false).await;
    let url = format!("http://{}", running.grpc);
    let channel = Channel::from_shared(url.clone())
```

with

```rust
#[tokio::test]
async fn accounts_are_served_only_with_the_workers_token() {
    let running = start().await;
    let url = format!("http://{}", running.grpc);
    let channel = Channel::from_shared(url.clone())
```

Replace

```rust
        .await
        .unwrap();
    let mut messaging = MessagingClient::connect(url.clone()).await.unwrap();
    let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
    let event = AccountDevice {
```

with

```rust
        .await
        .unwrap();
    let mut messaging = MessagingClient::connect(url).await.unwrap();
    let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
    let event = AccountDevice {
```

Replace

```rust
    let status = messaging.identify(identify).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::PermissionDenied);

    let mut setup = DevSetupClient::connect(url).await.unwrap();
    let status = setup
        .register_user(RegisterUserRequest {
            user_id: id_bytes(Uuid::now_v7()),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
```

with

```rust
    let status = messaging.identify(identify).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::PermissionDenied);
    running.shutdown.cancel();
    running.server.await.unwrap().unwrap();
```

Replace

```rust
#[tokio::test]
async fn the_client_requests_and_the_directory_run_on_postgres() {
    let running = start(false).await;
    let url = format!("http://{}", running.grpc);
    let mut accounts = accounts_client(url.clone()).await;
```

with

```rust
#[tokio::test]
async fn the_client_requests_and_the_directory_run_on_postgres() {
    let running = start().await;
    let url = format!("http://{}", running.grpc);
    let mut accounts = accounts_client(url.clone()).await;
```

The gateway's tests set up straight through message-svc's domain, which they already hold.
In `crates/push-gateway/tests/gateway.rs`:

Replace

```rust
use futures_util::{SinkExt, StreamExt};
use push_domain::{InMemoryStore, MAX_SYNC_BATCH, Messaging, SystemClock};
use push_gateway::{Config, serve};
use push_message_svc::{DevSetupApi, MessagingApi, RedisNotifier};
use push_proto::internal::v1::dev_setup_client::DevSetupClient;
use push_proto::internal::v1::dev_setup_server::DevSetupServer;
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::messaging_server::{self, MessagingServer};
use push_proto::internal::v1::{
    AckRequest, AckResponse, DeleteRequest, EditRequest, IdentifyRequest, IdentifyResponse,
    MakeContactsRequest, OpenDirectRequest, ReadUpToRequest, ReadUpToResponse,
    RegisterDeviceRequest, RegisterUserRequest, RequestCall, SendRequest, SyncRequest,
    SyncResponse,
};
use push_proto::v1::frame::Body;
```

with

```rust
use futures_util::{SinkExt, StreamExt};
use push_domain::{DeviceId, InMemoryStore, MAX_SYNC_BATCH, Messaging, SystemClock, UserId};
use push_gateway::{Config, serve};
use push_message_svc::{MessagingApi, RedisNotifier};
use push_proto::internal::v1::messaging_client::MessagingClient;
use push_proto::internal::v1::messaging_server::{self, MessagingServer};
use push_proto::internal::v1::{
    AckRequest, AckResponse, DeleteRequest, EditRequest, IdentifyRequest, IdentifyResponse,
    ReadUpToRequest, ReadUpToResponse, RequestCall, SendRequest, SyncRequest, SyncResponse,
};
use push_proto::v1::frame::Body;
```

Replace

```rust
struct Stack {
    _redis: ContainerAsync<Redis>,
    message_svc: String,
    gateway: SocketAddr,
```

with

```rust
struct Stack {
    _redis: ContainerAsync<Redis>,
    /// message-svc's domain, to set up users, contacts and conversations directly.
    messaging: Arc<Messaging<InMemoryStore, SystemClock, RedisNotifier>>,
    message_svc: String,
    gateway: SocketAddr,
```

Replace

```rust
                sync_failures: Mutex::new(sync_failures),
            }))
            .add_service(DevSetupServer::new(DevSetupApi::new(messaging)))
            .serve_with_incoming(TcpListenerStream::new(grpc)),
    );
```

with

```rust
                sync_failures: Mutex::new(sync_failures),
            }))
            .serve_with_incoming(TcpListenerStream::new(grpc)),
    );
```

Replace

```rust
    Stack {
        _redis: redis,
        message_svc,
        gateway,
```

with

```rust
    Stack {
        _redis: redis,
        messaging,
        message_svc,
        gateway,
```

Replace

```rust
}

/// A user with one device, registered through the dev setup API.
async fn person(stack: &Stack) -> (Uuid, Uuid) {
    let mut setup = DevSetupClient::connect(stack.message_svc.clone())
        .await
        .unwrap();
    let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
    setup
        .register_user(RegisterUserRequest {
            user_id: id_bytes(user),
        })
        .await
        .unwrap();
    setup
        .register_device(RegisterDeviceRequest {
            user_id: id_bytes(user),
            device_id: id_bytes(device),
        })
        .await
        .unwrap();
```

with

```rust
}

/// A user with one device, relayed to message-svc as the worker relays them.
async fn person(stack: &Stack) -> (Uuid, Uuid) {
    let (user, device) = (Uuid::now_v7(), Uuid::now_v7());
    stack
        .messaging
        .device_registered(DeviceId(device), UserId(user))
        .await
        .unwrap();
```

Replace

```rust
}

async fn connect(stack: &Stack) -> Socket {
    let (socket, _) = connect_async(format!("ws://{}/v1/socket", stack.gateway))
```

with

```rust
}

/// Makes two users contacts and opens their direct conversation.
async fn direct_chat(stack: &Stack, alice: Uuid, bob: Uuid) -> Vec<u8> {
    let (alice, bob) = (UserId(alice), UserId(bob));
    stack.messaging.contact_request(alice, bob).await.unwrap();
    stack.messaging.contact_accept(bob, alice).await.unwrap();
    let conversation = stack.messaging.open_direct(alice, bob).await.unwrap();
    id_bytes(conversation.0)
}

async fn connect(stack: &Stack) -> Socket {
    let (socket, _) = connect_async(format!("ws://{}/v1/socket", stack.gateway))
```

Replace

```rust
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
    let mut setup = DevSetupClient::connect(stack.message_svc.clone())
        .await
        .unwrap();
    setup
        .make_contacts(MakeContactsRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap();
    let conversation = setup
        .open_direct(OpenDirectRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap()
        .into_inner()
        .conversation_id;

    let mut bob_socket = connect(&stack).await;
```

with

```rust
    let (alice, alice_device) = person(&stack).await;
    let (bob, bob_device) = person(&stack).await;
    let conversation = direct_chat(&stack, alice, bob).await;

    let mut bob_socket = connect(&stack).await;
```

Replace

```rust
    let (alice, alice_device) = person(stack).await;
    let (bob, bob_device) = person(stack).await;
    let mut setup = DevSetupClient::connect(stack.message_svc.clone())
        .await
        .unwrap();
    setup
        .make_contacts(MakeContactsRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap();
    let conversation = setup
        .open_direct(OpenDirectRequest {
            user_id: id_bytes(alice),
            peer_id: id_bytes(bob),
        })
        .await
        .unwrap()
        .into_inner()
        .conversation_id;
    let mut messaging = MessagingClient::connect(stack.message_svc.clone())
        .await
```

with

```rust
    let (alice, alice_device) = person(stack).await;
    let (bob, bob_device) = person(stack).await;
    let conversation = direct_chat(stack, alice, bob).await;

    let mut messaging = MessagingClient::connect(stack.message_svc.clone())
        .await
```

- [ ] **Step 2: Remove it**

In `crates/push-proto/proto/push/internal/v1/messaging.proto`:

Replace

```protobuf
}

// DEV ONLY: setup calls for local clusters and the smoke test, registered only by a
// message-svc started with PUSH_DEV_SETUP=1. Plans 5-7 replace them with auth-svc events
// and the real contact and conversation requests.
service DevSetup {
  rpc RegisterUser(RegisterUserRequest) returns (RegisterUserResponse);
  rpc RegisterDevice(RegisterDeviceRequest) returns (RegisterDeviceResponse);
  // Makes two users accepted contacts: a request from `user_id`, accepted by `peer_id`.
  rpc MakeContacts(MakeContactsRequest) returns (MakeContactsResponse);
  rpc OpenDirect(OpenDirectRequest) returns (OpenDirectResponse);
}

// auth-svc's user and device events, relayed by the worker from auth-svc's outbox
// (spec §2, §5.3). Delivery is at least once and in no particular order, so every call is
```

with

```protobuf
}

// auth-svc's user and device events, relayed by the worker from auth-svc's outbox
// (spec §2, §5.3). Delivery is at least once and in no particular order, so every call is
```

Replace

```protobuf
}

message RegisterUserRequest {
  bytes user_id = 1;
}

message RegisterUserResponse {}

message RegisterDeviceRequest {
  bytes user_id = 1;
  bytes device_id = 2;
}

message RegisterDeviceResponse {}

message MakeContactsRequest {
  bytes user_id = 1;
  bytes peer_id = 2;
}

message MakeContactsResponse {}

message OpenDirectRequest {
  bytes user_id = 1;
  bytes peer_id = 2;
}

message OpenDirectResponse {
  bytes conversation_id = 1;
}

message AccountUser {
  bytes user_id = 1;
```

with

```protobuf
}

message AccountUser {
  bytes user_id = 1;
```

In `crates/push-message-svc/src/config.rs` (`PUSH_DEV_SETUP`, its field, its test and the
`dev_flag` helper it alone used):

Replace

```rust
/// | `PUSH_ACCOUNTS_TOKEN` | the worker's token for the `Accounts` API, 32 to 256 visible ASCII characters; without it `Accounts` refuses every call | none |
/// | `PUSH_DIRECTORY_TOKEN` | auth-svc's token for the `Directory` API, 32 to 256 visible ASCII characters; without it `Directory` refuses every call | none |
/// | `PUSH_DEV_SETUP` | `1` registers the DEV ONLY `DevSetup` service | off |
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
```

with

```rust
/// | `PUSH_ACCOUNTS_TOKEN` | the worker's token for the `Accounts` API, 32 to 256 visible ASCII characters; without it `Accounts` refuses every call | none |
/// | `PUSH_DIRECTORY_TOKEN` | auth-svc's token for the `Directory` API, 32 to 256 visible ASCII characters; without it `Directory` refuses every call | none |
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
```

Replace

```rust
    pub accounts_token: Option<String>,
    pub directory_token: Option<String>,
    pub dev_setup: bool,
}
```

with

```rust
    pub accounts_token: Option<String>,
    pub directory_token: Option<String>,
}
```

Replace

```rust
                &self.directory_token.as_ref().map(|_| "<redacted>"),
            )
            .field("dev_setup", &self.dev_setup)
            .finish()
    }
```

with

```rust
                &self.directory_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
```

Replace

```rust
            accounts_token,
            directory_token,
            dev_setup: dev_flag(&lookup, "PUSH_DEV_SETUP")?,
        })
    }
```

with

```rust
            accounts_token,
            directory_token,
        })
    }
```

Replace

```rust
}

/// A DEV ONLY switch: on only for exactly `1`; off when unset, empty or `0`. Anything
/// else (`true`, `yes`) is refused, so a typo can never switch it on or leave it in doubt.
fn dev_flag(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<bool, ConfigError> {
    match lookup(name).as_deref() {
        None | Some("") | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(ConfigError::Invalid {
            name,
            value: other.to_owned(),
            reason: "use 1 to enable or 0 to disable".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
```

with

```rust
}

#[cfg(test)]
mod tests {
```

Replace

```rust
        assert_eq!(config.accounts_token, None);
        assert_eq!(config.directory_token, None);
        assert!(!config.dev_setup);
    }
```

with

```rust
        assert_eq!(config.accounts_token, None);
        assert_eq!(config.directory_token, None);
    }
```

Replace

```rust
    }

    #[test]
    fn dev_setup_is_on_only_for_exactly_1() {
        for (value, expected) in [("1", true), ("0", false), ("", false)] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_DEV_SETUP", value));
            assert_eq!(config(&variables).unwrap().dev_setup, expected, "{value:?}");
        }
        for value in ["true", "yes", " 1"] {
            let mut variables = REQUIRED.to_vec();
            variables.push(("PUSH_DEV_SETUP", value));
            assert!(
                matches!(
                    config(&variables),
                    Err(ConfigError::Invalid {
                        name: "PUSH_DEV_SETUP",
                        ..
                    })
                ),
                "{value:?}"
            );
        }
    }

    #[test]
    fn unusable_values_fail_start_up_without_echoing_the_urls() {
```

with

```rust
    }

    #[test]
    fn unusable_values_fail_start_up_without_echoing_the_urls() {
```

In `crates/push-message-svc/src/server.rs`:

Replace

```rust
use push_domain::{Messaging, SystemClock};
use push_proto::internal::v1::accounts_server::AccountsServer;
use push_proto::internal::v1::dev_setup_server::DevSetupServer;
use push_proto::internal::v1::directory_server::DirectoryServer;
use push_proto::internal::v1::messaging_server::MessagingServer;
```

with

```rust
use push_domain::{Messaging, SystemClock};
use push_proto::internal::v1::accounts_server::AccountsServer;
use push_proto::internal::v1::directory_server::DirectoryServer;
use push_proto::internal::v1::messaging_server::MessagingServer;
```

Replace

```rust
use crate::config::Config;
use crate::grpc::{AccountsApi, DevSetupApi, DirectoryApi, InternalToken, MessagingApi};
use crate::notify::RedisNotifier;
```

with

```rust
use crate::config::Config;
use crate::grpc::{AccountsApi, DirectoryApi, InternalToken, MessagingApi};
use crate::notify::RedisNotifier;
```

Replace

```rust
    );

    if config.dev_setup {
        tracing::warn!(
            "DEV ONLY: PUSH_DEV_SETUP=1 registers the DevSetup service, which creates users, \
             devices, contacts and conversations without authentication. Never enable it \
             outside a local cluster."
        );
    }
    if config.accounts_token.is_none() {
        tracing::warn!(
```

with

```rust
    );

    if config.accounts_token.is_none() {
        tracing::warn!(
```

Replace

```rust
    // waits in the listen backlog until the server accepts, a moment later.
    let messaging = Arc::new(Messaging::new(store.clone(), SystemClock, notifier));
    let dev_setup = config
        .dev_setup
        .then(|| DevSetupServer::new(DevSetupApi::new(Arc::clone(&messaging))));
    tracing::info!(addr = %config.grpc_addr, "serving gRPC");
    let result = tonic::transport::Server::builder()
```

with

```rust
    // waits in the listen backlog until the server accepts, a moment later.
    let messaging = Arc::new(Messaging::new(store.clone(), SystemClock, notifier));

    tracing::info!(addr = %config.grpc_addr, "serving gRPC");
    let result = tonic::transport::Server::builder()
```

Replace

```rust
            InternalToken::directory(config.directory_token.clone()),
        ))
        .add_optional_service(dev_setup)
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(grpc),
```

with

```rust
            InternalToken::directory(config.directory_token.clone()),
        ))
        .serve_with_incoming_shutdown(
            TcpListenerStream::new(grpc),
```

In `crates/push-message-svc/src/lib.rs`:

Replace

```rust
pub use config::{Config, ConfigError};
pub use grpc::{
    AccountsApi, DevSetupApi, DirectoryApi, InternalToken, MAX_VISIBLE_USERS, MessagingApi,
};
pub use notify::RedisNotifier;
pub use server::{init_logging, serve, terminated};
```

with

```rust
pub use config::{Config, ConfigError};
pub use grpc::{AccountsApi, DirectoryApi, InternalToken, MAX_VISIBLE_USERS, MessagingApi};
pub use notify::RedisNotifier;
pub use server::{init_logging, serve, terminated};
```

In `crates/push-smoke/tests/local.rs`:

Replace

```rust
            accounts_token: Some(accounts_token.clone()),
            directory_token: Some(directory_token.to_owned()),
            dev_setup: false,
        },
        grpc,
```

with

```rust
            accounts_token: Some(accounts_token.clone()),
            directory_token: Some(directory_token.to_owned()),
        },
        grpc,
```

```powershell
cargo test -p push-message-svc -p push-gateway -p push-smoke
git grep -n -i "devsetup\|dev_setup" -- crates
```

Expected: everything passes; `git grep` finds nothing.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add crates
git commit -m "refactor(message-svc): remove the DEV ONLY DevSetup API"
```

Expected: 807 tests pass (the `PUSH_DEV_SETUP` configuration test is gone), no warnings,
no diff.

---

### Task 4: The chart: the Directory token, auth-svc's message-svc URL, the smoke test's Redis URL

**Files:**
- Modify: `deploy/helm/push-messenger/templates/secret.yaml`, `templates/auth.yaml`,
  `templates/message-svc.yaml`, `templates/tests/smoke.yaml`, `templates/_helpers.tpl`,
  `templates/NOTES.txt`, `values.yaml`, `deploy/helm/test-chart.ps1`

- [ ] **Step 1: The checks, failing**

In `deploy/helm/test-chart.ps1`:

Replace

```powershell
$default = helm template push $chart | Out-String
Check 'default: the gateway refuses dev identities' { $default -match 'name: PUSH_DEV_AUTH\s+value: "0"' }
Check 'default: message-svc has no DevSetup API' { $default -match 'name: PUSH_DEV_SETUP\s+value: "0"' }
Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' }
Check 'default: the worker has no dev mailbox and logs email; auth-svc has no mailbox at all' {
```

with

```powershell
$default = helm template push $chart | Out-String
Check 'default: the gateway refuses dev identities' { $default -match 'name: PUSH_DEV_AUTH\s+value: "0"' }
Check 'default: the DevSetup switch is gone' { $default -notmatch 'PUSH_DEV_SETUP' }
Check 'default: no smoke test' { $default -notmatch 'helm.sh/hook: test' }
Check 'default: the worker has no dev mailbox and logs email; auth-svc has no mailbox at all' {
```

Replace

```powershell
        $gateway -notmatch 'accounts-token' -and -not $messageSvc.Contains($token) -and -not $worker.Contains($token)
}
Check 'default: auth-svc and the worker drain like the gateway (preStop sleep, grace period)' {
    $auth = Doc $default 'Deployment' 'push-auth'
```

with

```powershell
        $gateway -notmatch 'accounts-token' -and -not $messageSvc.Contains($token) -and -not $worker.Contains($token)
}
Check 'default: auth-svc and message-svc share the Directory token from the Secret, nobody else has it' {
    $auth = Doc $default 'Deployment' 'push-auth'
    $messageSvc = Doc $default 'Deployment' 'push-message-svc'
    $token = [regex]::Match($default, 'directory-token: "([^"]+)"').Groups[1].Value
    $token.Length -eq 48 -and
        $auth -match 'name: PUSH_DIRECTORY_TOKEN\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: directory-token' -and
        $messageSvc -match 'name: PUSH_DIRECTORY_TOKEN\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: directory-token' -and
        $auth -match 'name: PUSH_MESSAGE_SVC_URL\s+value: "http://push-message-svc:50051"' -and
        $auth -notmatch 'accounts-token' -and
        (Doc $default 'Deployment' 'push-worker') -notmatch 'directory-token' -and
        (Doc $default 'Deployment' 'push-gateway') -notmatch 'directory-token' -and
        -not $auth.Contains($token) -and -not $messageSvc.Contains($token)
}
Check 'default: auth-svc and the worker drain like the gateway (preStop sleep, grace period)' {
    $auth = Doc $default 'Deployment' 'push-auth'
```

Replace

```powershell
$dev = helm template push $chart -f $minikube | Out-String
Check 'minikube: dev identities on' { $dev -match 'name: PUSH_DEV_AUTH\s+value: "1"' }
Check 'minikube: DevSetup API on' { $dev -match 'name: PUSH_DEV_SETUP\s+value: "1"' }
Check 'minikube: the worker''s dev mailbox on' {
    (Doc $dev 'Deployment' 'push-worker') -match 'name: PUSH_DEV_MAILBOX\s+value: "1"'
```

with

```powershell
$dev = helm template push $chart -f $minikube | Out-String
Check 'minikube: dev identities on' { $dev -match 'name: PUSH_DEV_AUTH\s+value: "1"' }
Check 'minikube: the smoke test calls message-svc and reads the Redis URL from the Secret' {
    $smoke = Doc $dev 'Pod' 'push-smoke'
    $smoke -match '- --message-svc\s+- "http://push-message-svc:50051"' -and
        $smoke -match 'name: PUSH_SMOKE_REDIS_URL\s+valueFrom:\s+secretKeyRef:\s+name: push-secrets\s+key: redis-url' -and
        $smoke -notmatch 'redis://' -and $dev -notmatch 'PUSH_DEV_SETUP'
}
Check 'minikube: the worker''s dev mailbox on' {
    (Doc $dev 'Deployment' 'push-worker') -match 'name: PUSH_DEV_MAILBOX\s+value: "1"'
```

```powershell
pwsh deploy/helm/test-chart.ps1
```

Expected: `FAIL default: the DevSetup switch is gone`, `FAIL default: auth-svc and
message-svc share the Directory token from the Secret, nobody else has it` and `FAIL
minikube: the smoke test calls message-svc and reads the Redis URL from the Secret`, then
`3 check(s) failed`.

- [ ] **Step 2: The token, the URLs and the smoke Pod**

In `deploy/helm/push-messenger/templates/secret.yaml` (generated once, kept like the
Accounts token):

Replace

```yaml
{{- $workerPassword := .Values.postgres.workerPassword | default (get $existing "worker-db-password" | b64dec) | default (randAlphaNum 32) -}}
{{- $accountsToken := get $existing "accounts-token" | b64dec | default (randAlphaNum 48) -}}
{{- /* Resend's credentials come from values (Plan 11: Key Vault); a local cluster gets a
generated webhook secret, so the webhook path can be tried with signed requests. */ -}}
```

with

```yaml
{{- $workerPassword := .Values.postgres.workerPassword | default (get $existing "worker-db-password" | b64dec) | default (randAlphaNum 32) -}}
{{- $accountsToken := get $existing "accounts-token" | b64dec | default (randAlphaNum 48) -}}
{{- $directoryToken := get $existing "directory-token" | b64dec | default (randAlphaNum 48) -}}
{{- /* Resend's credentials come from values (Plan 11: Key Vault); a local cluster gets a
generated webhook secret, so the webhook path can be tried with signed requests. */ -}}
```

Replace

```yaml
  # message-svc too, does not get it, so it cannot register devices.
  accounts-token: {{ $accountsToken | quote }}
  {{- with $resendApiKey }}
  resend-api-key: {{ . | quote }}
```

with

```yaml
  # message-svc too, does not get it, so it cannot register devices.
  accounts-token: {{ $accountsToken | quote }}
  # auth-svc's token for message-svc's Directory API (whose profiles a user may see). Only
  # auth-svc and message-svc get it; with the Accounts token it cannot register devices.
  directory-token: {{ $directoryToken | quote }}
  {{- with $resendApiKey }}
  resend-api-key: {{ . | quote }}
```

In `deploy/helm/push-messenger/templates/auth.yaml`:

Replace

```yaml
            - name: PUSH_AUTH_WORKER_ROLE
              value: push_worker
          ports:
            - name: http
```

with

```yaml
            - name: PUSH_AUTH_WORKER_ROLE
              value: push_worker
            # GET /v1/users?ids=… asks message-svc's Directory API whose profiles the
            # caller may see, with auth-svc's own token.
            - name: PUSH_MESSAGE_SVC_URL
              value: {{ printf "http://%s:50051" (include "push.name" (dict "root" . "component" "message-svc")) | quote }}
            - name: PUSH_DIRECTORY_TOKEN
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: directory-token
          ports:
            - name: http
```

In `deploy/helm/push-messenger/templates/message-svc.yaml`:

Replace

```yaml
                  name: {{ include "push.secretName" . }}
                  key: accounts-token
            - name: PUSH_DEV_SETUP
              value: {{ include "push.devFlag" . | quote }}
          ports:
            - name: grpc
```

with

```yaml
                  name: {{ include "push.secretName" . }}
                  key: accounts-token
            # Only auth-svc has this one: the Directory API refuses calls without it.
            - name: PUSH_DIRECTORY_TOKEN
              valueFrom:
                secretKeyRef:
                  name: {{ include "push.secretName" . }}
                  key: directory-token
          ports:
            - name: grpc
```

In `deploy/helm/push-messenger/templates/tests/smoke.yaml`:

Replace

```yaml
# API and the WebSockets go through the ingress controller with the ingress host as the
# Host header, as a client's would; the DEV ONLY mailbox and the calls to message-svc go
# straight to the worker and message-svc, which the ingress does not expose.
apiVersion: v1
kind: Pod
```

with

```yaml
# API and the WebSockets go through the ingress controller with the ingress host as the
# Host header, as a client's would; the DEV ONLY mailbox and the calls to message-svc go
# straight to the worker and message-svc, which the ingress does not expose. It listens on
# Redis for the kicks message-svc publishes, with the Redis URL from the Secret.
apiVersion: v1
kind: Pod
```

Replace

```yaml
        - --mailbox
        - {{ printf "http://%s:8080" (include "push.name" (dict "root" . "component" "worker")) | quote }}
        - --setup
        - {{ printf "http://%s:50051" (include "push.name" (dict "root" . "component" "message-svc")) | quote }}
        - --socket
```

with

```yaml
        - --mailbox
        - {{ printf "http://%s:8080" (include "push.name" (dict "root" . "component" "worker")) | quote }}
        - --message-svc
        - {{ printf "http://%s:50051" (include "push.name" (dict "root" . "component" "message-svc")) | quote }}
        - --socket
```

Replace

```yaml
        - --host
        - {{ .Values.ingress.host | quote }}
      resources:
        requests:
```

with

```yaml
        - --host
        - {{ .Values.ingress.host | quote }}
      env:
        - name: PUSH_SMOKE_REDIS_URL
          valueFrom:
            secretKeyRef:
              name: {{ include "push.secretName" . }}
              key: redis-url
      resources:
        requests:
```

The comments that listed `DevSetup`. In `deploy/helm/push-messenger/values.yaml`:

Replace

```yaml
buildId: ""

# DEV ONLY switches: the gateway accepts identities without a token (PUSH_DEV_AUTH=1),
# message-svc serves the DevSetup API (PUSH_DEV_SETUP=1), the worker shows the emails its
# log sender delivered at /dev/mailbox (PUSH_DEV_MAILBOX=1), a webhook secret is generated
# when none is set, and `helm test` runs the smoke test. Never enable outside a local
# cluster.
devMode:
  enabled: false
```

with

```yaml
buildId: ""

# DEV ONLY switches: the gateway accepts identities without a token (PUSH_DEV_AUTH=1), the
# worker shows the emails its log sender delivered at /dev/mailbox (PUSH_DEV_MAILBOX=1), a
# webhook secret is generated when none is set, and `helm test` runs the smoke test. Never
# enable outside a local cluster.
devMode:
  enabled: false
```

In `deploy/helm/push-messenger/templates/_helpers.tpl`:

Replace

```text
{{- end -}}

{{/* "1" or "0" for the PUSH_DEV_AUTH, PUSH_DEV_SETUP and PUSH_DEV_MAILBOX variables. */}}
{{- define "push.devFlag" -}}
{{- if include "push.devMode" . -}}1{{- else -}}0{{- end -}}
```

with

```text
{{- end -}}

{{/* "1" or "0" for the PUSH_DEV_AUTH and PUSH_DEV_MAILBOX variables. */}}
{{- define "push.devFlag" -}}
{{- if include "push.devMode" . -}}1{{- else -}}0{{- end -}}
```

In `deploy/helm/push-messenger/templates/NOTES.txt`:

Replace

```text
{{- if include "push.devMode" . }}

DEV MODE IS ON: the gateway accepts identities without a token, message-svc serves the
DevSetup API and the worker shows the emails its log sender delivered at /dev/mailbox (not
routed by the ingress). This is for a local cluster only.

Run the end-to-end smoke test:
```

with

```text
{{- if include "push.devMode" . }}

DEV MODE IS ON: the gateway accepts identities without a token and the worker shows the
emails its log sender delivered at /dev/mailbox (not routed by the ingress). This is for a
local cluster only.

Run the end-to-end smoke test:
```

```powershell
pwsh deploy/helm/test-chart.ps1
```

Expected: 29 lines starting `ok`, then `all chart checks passed`.

- [ ] **Step 3: Run the gates and commit**

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo +1.85 build --workspace --all-targets --locked
git add deploy
git commit -m "feat(chart): the Directory token, auth-svc's message-svc URL and the smoke test's Redis URL"
```

Expected: 807 tests pass, no warnings, no diff.

---

### Task 5: Bring it up on minikube and run the smoke test

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

If it stops with "The Postgres volume predates auth-svc or the worker … Run
deploy/minikube/down.ps1 -Cluster push first" (a volume from before Plan 6B, or one a
cluster restart left behind; it happened while this plan was verified), run
`deploy/minikube/down.ps1` and then `deploy/minikube/up.ps1 -Smoke` again.

Expected (verified 2026-10-14; ids and timings differ): the five images build, the release
installs, and `helm test` ends `Phase: Succeeded` with these pod logs:

```
==> build push-messenger/auth:dev
==> build push-messenger/message-svc:dev
==> build push-messenger/gateway:dev
==> build push-messenger/worker:dev
==> build push-messenger/smoke:dev
==> install release 'push' in namespace 'push' (attempt 1 of 5)
Release "push" does not exist. Installing it now.
STATUS: deployed
==> run the smoke test
TEST SUITE:     push-smoke
Phase:          Succeeded
POD LOGS: push-smoke
smoke: auth-svc answers 503 Service Unavailable yet; retrying
smoke: auth-svc answers 503 Service Unavailable yet; retrying
smoke: alice signed up; the worker's log sender delivered the verification email after 759 ms
smoke: alice verified the email, signed in and registered device 01a13798-a47b-722b-b43d-34f6044b6c4c (a retry returned the same device) in 833 ms
smoke: alice's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: bob signed up; the worker's log sender delivered the verification email after 1008 ms
smoke: bob verified the email, signed in and registered device 01a13798-a8a3-73e3-a658-ef606634797d (a retry returned the same device) in 1059 ms
smoke: bob's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: carol signed up; the worker's log sender delivered the verification email after 1013 ms
smoke: carol verified the email, signed in and registered device 01a13798-accf-7018-94d9-c0ff2611b5ce (a retry returned the same device) in 1065 ms
smoke: carol's access token verifies against the JWKS (EdDSA, aud ws://push.local/v1/socket); refresh rotated it
smoke: the worker relayed alice's account and device 01a13798-a47b-722b-b43d-34f6044b6c4c to message-svc (waited 0 ms)
smoke: the worker relayed bob's account and device 01a13798-a8a3-73e3-a658-ef606634797d to message-svc (waited 0 ms)
smoke: the worker relayed carol's account and device 01a13798-accf-7018-94d9-c0ff2611b5ce to message-svc (waited 1022 ms)
smoke: alice asked bob to be a contact and bob accepted
smoke: carol redeemed alice's invite: they are contacts
smoke: alice opened the direct conversation with bob
smoke: auth-svc shows alice the profiles of her contacts bob and carol, and bob none of carol's
smoke: bob connected (Ready, heartbeat 30000 ms)
smoke: alice connected (Ready, heartbeat 30000 ms)
smoke: alice got SendAck seq=1 after 23 ms
smoke: bob got Deliver seq=1 after 36 ms
smoke: bob acked; the gateway answered the following Ping
smoke: alice edited her message; bob's socket delivered the edit (seq 2)
smoke: bob read the message; alice's listing shows it read
smoke: alice created a group with bob and carol; bob's socket delivered its first entry and auth-svc now shows bob carol's profile
smoke: alice deleted her message; bob's socket delivered the delete (seq 3) and his history holds it erased
smoke: carol revoked her device; the worker relayed it and message-svc kicked it on Redis after 386 ms, and refuses it now
smoke: bob reset his password; the worker relayed it and message-svc kicked his device on Redis after 725 ms
smoke: PASS in 10868 ms
```

- [ ] **Step 3: An upgrade keeps the Directory token**

```powershell
$before = kubectl --context push -n push get secret push-secrets -o jsonpath='{.data.directory-token}'
deploy/minikube/up.ps1 -SkipBuild -Smoke
$after = kubectl --context push -n push get secret push-secrets -o jsonpath='{.data.directory-token}'
"directory-token kept: $($before -eq $after -and $before.Length -gt 0)"
```

Expected: `REVISION: 2`, `Phase: Succeeded`, `smoke: PASS in 6212 ms` (or similar) and
`directory-token kept: True`.

- [ ] **Step 4: If anything fails**

`kubectl --context push -n push get pods`, `kubectl --context push -n push logs
deploy/push-message-svc` (or `push-auth`, `push-worker`, `push-gateway`) and
`helm test push -n push --logs` show where. Fix it in the task that introduced it, run that
task's gates, and repeat Step 2.

- [ ] **Step 5: Stop the cluster and restore the context**

```powershell
minikube stop -p push
kubectl config use-context minikube
kubectl config current-context
```

Expected: `1 node stopped.` and `minikube`. Never delete the profile.

---

### Task 6: CI, README, spec, roadmap and pull request

**Files:**
- Modify: `README.md`, `docs/superpowers/specs/2026-10-11-push-messenger-design.md`,
  `docs/superpowers/plans/2026-10-11-roadmap.md`

CI needs no change: the `images` job already builds the five images, and the `chart` job
runs `test-chart.ps1`, now with 29 checks. The minikube run stays the merge gate, recorded
in the pull request.

- [ ] **Step 1: README**

In `README.md`, in the paragraph after `deploy/minikube/up.ps1 -Smoke`, replace

```markdown
`values-minikube.yaml`, and runs the end-to-end smoke test: two users sign up, verify their
email (the worker's log sender delivers it), register a device, wait for the worker to
relay it to message-svc, and chat through the ingress; the DEV ONLY mailbox and the setup
calls go to the services directly. `deploy/minikube/up.sh --smoke` does the same on Linux
```

with

```markdown
`values-minikube.yaml`, and runs the end-to-end smoke test: three users sign up, verify
their email (the worker's log sender delivers it), register a device and wait for the worker
to relay it to message-svc; they become contacts by request and by invite, see only each
other's allowed profiles, chat through the ingress, edit, read, form a group and delete,
and finally a revoked device and a password reset are kicked on Redis. The DEV ONLY mailbox
and the calls to message-svc's API (the gateway carries them from Plan 8) go to the
services directly. `deploy/minikube/up.sh --smoke` does the same on Linux
```

In the paragraph that starts `The minikube values switch on DEV ONLY shortcuts`, replace

```markdown
id without a token (until it checks access tokens, Plan 8), message-svc serves a setup API
for contacts and conversations (until Plan 7), and the worker, which logs emails instead of
sending them, shows them at `/dev/mailbox`; none of these is routed by the ingress, which
```

with

```markdown
id without a token (until it checks access tokens, Plan 8), and the worker, which logs
emails instead of sending them, shows them at `/dev/mailbox`; neither is routed by the
ingress, which
```

In the paragraph that starts `auth-svc and the worker have databases and roles of their
own`, replace

```markdown
message-svc and the worker share a generated Accounts token from the same Secret, which
the gateway does not get. A
```

with

```markdown
message-svc and the worker share a generated Accounts token from the same Secret, and
message-svc and auth-svc a Directory token; the gateway gets neither. A
```

In the `push-message-svc` table, delete the row for `PUSH_DEV_SETUP`.

- [ ] **Step 2: The spec**

In §7.6, replace the bullet that starts `- **DEV ONLY identity, until real tokens (Plans 5
and 8).**` with:

```markdown
- **DEV ONLY identity, until real tokens (Plan 8).** A gateway started with
  `PUSH_DEV_AUTH=1` accepts `Identify{dev_user_id, dev_device_id}` without a token;
  otherwise `Identify` closes with 4001. The variable accepts only `1` or `0`, and the
  gateway logs a warning at start-up when it is enabled. When the dev fields are removed
  (Plan 8), their field numbers (14, 15) and names are reserved. message-svc's DEV ONLY
  `DevSetup` service and `PUSH_DEV_SETUP` were removed in Plan 7B: users and devices come
  from the worker's `Accounts` calls, contacts and conversations from the request frames.
```

In §11, after the bullet that starts `- The worker (Plan 6B) is a fourth Deployment`, add:

```markdown
- Plan 7B: the Secret also holds `directory-token`, auth-svc's token for message-svc's
  `Directory` API (generated and kept; auth-svc and message-svc read it, the worker and the
  gateway do not), and auth-svc gets `PUSH_MESSAGE_SVC_URL`. `PUSH_DEV_SETUP` is gone.
  The smoke Pod reads the Redis URL from the Secret (`PUSH_SMOKE_REDIS_URL`) to hear kicks.
```

In §11, in the last bullet (it starts `` - `deploy/minikube/up.ps1` (and `up.sh`) wait for
ingress-nginx``), replace

```markdown
  `push-smoke` inside the cluster through the ingress controller: two users sign up, verify
  with the emails the worker's log sender delivered to the DEV ONLY mailbox, register
  devices, wait for the worker to relay them to message-svc, and chat. The Azure plan
```

with

```markdown
  `push-smoke` inside the cluster through the ingress controller: three users sign up,
  verify with the emails the worker's log sender delivered to the DEV ONLY mailbox,
  register devices and wait for the worker to relay them to message-svc; they become
  contacts by request and invite, see only allowed profiles, chat, edit, read, form a group
  and delete over message-svc's API and the gateway, and a revoked device and a password
  reset are kicked on Redis (Plan 7B). The Azure plan
```

- [ ] **Step 3: Mark plan 7B in review**

In `docs/superpowers/plans/2026-10-11-roadmap.md`, replace the empty status cell at the end
of the row for plan 7B (`| 7B | Message service: minikube | … | 7A | |`) with `In review`.

- [ ] **Step 4: Final gates**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.85 test --workspace --locked
pwsh deploy/helm/test-chart.ps1
```

Expected: no diff, no warnings, 807 tests on both toolchains, 29 chart checks.

- [ ] **Step 5: Commit and push**

```powershell
git add README.md docs
git commit -m "docs: record plan 7B in the spec, README and roadmap"
git push -u origin feature/plan-07b-message-minikube
```

- [ ] **Step 6: Open the pull request and wait for CI**

```powershell
$body = @'
Implements docs/superpowers/plans/2026-10-13-plan-07b-message-minikube.md: the smoke test drives Plan 7A's API end to end — contacts by request and by invite, the direct conversation, profiles only for related users through auth-svc's GET /v1/users?ids=, edit, read, a group and a delete delivered to Bob's open WebSocket, history with the message erased, and kicks on Redis after a device revocation and a password reset relayed by the worker. The DEV ONLY DevSetup API is removed. The chart generates and keeps a Directory token for auth-svc and message-svc only, gives auth-svc message-svc's URL, and gives the smoke Pod the Redis URL from the Secret; test-chart.ps1 has 29 checks. Verified on minikube (profile push, Kubernetes 1.31): smoke PASS, and an upgrade keeps the token. 807 tests.
'@
gh pr create --base main --title "Plan 7B: the message service on minikube" --body $body
gh pr checks --watch
```

Expected: the `ci` workflow succeeds (`rust`, `msrv`, `images`, `chart`). Do not merge; the
controller merges after the final review.

---

## Spec coverage for this plan

| Spec item / decision | Task |
|---|---|
| §2 each internal API with its own token; auth-svc never reads message-svc's database | 1, 4 |
| §5.3, §5.4 kicks on a relayed revocation and password reset, end to end | 2, 5 |
| §5.5 contacts by request and invite; profiles only for shared contacts and conversations | 1, 2, 5 |
| §4.6, §6 edits, deletes and groups reach a connected device | 2, 5 |
| §7.6 the DEV ONLY setup API removed | 3, 4 |
| §11 secrets generated and kept, never in a ConfigMap or an argument; verified on minikube | 4, 5 |
| §12 end-to-end: invite, chat, receipts, edit, delete, revoked device | 1, 2, 5 |

## Deferred to later plans

- **The gateway carries the new frames** (Plan 8); the smoke test then sends them over the
  WebSocket instead of calling message-svc, and checks that kicked sockets close (4003, and
  the code owner decision 1 of Plan 7A picks for a password reset).
- **The smoke test's Redis access** goes when the gateway closes kicked sockets (Plan 8):
  it will watch the socket close instead.
- **A NetworkPolicy** (Plan 11): only auth-svc may reach `Directory`, only the worker
  `Accounts`, only the gateway `Messaging`, and the smoke Pod only in dev mode.
- **Per-IP rate limits on `RedeemInvite`** (Plan 8, at the gateway).
