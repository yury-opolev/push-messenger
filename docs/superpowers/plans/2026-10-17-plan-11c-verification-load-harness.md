# Plan 11C: Reusable Verification and Bounded Load Harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Work sequentially on one branch.

**Goal:** Give consumers documented, registry-neutral build/package/verification commands and `push-load`, an explicitly targeted, bounded harness that prepares supplied test accounts and measures authenticated connections, heartbeats, message/receipt latency and reconnects.

**Architecture:** A new Rust crate `crates\push-load` (library + `push-load` binary) speaks only the public API: auth-svc's HTTP endpoints and the gateway's WebSocket protocol from `push-proto`. `prepare` signs in operator-supplied verified accounts, registers devices and makes sample pairs contacts, resumably, into a private state file. `run` refreshes tokens once, ramps connections at a fixed rate for a finite duration, and writes a credential-free JSON report whose status is `passed` only when every criterion holds. A DEV ONLY account helper stays in `push-smoke`; production tools never need the signing key, the mailbox or disabled rate limits.

**Tech Stack:** Rust 2024 / MSRV 1.85, Tokio, `tokio-tungstenite` and `reqwest` with rustls (as `push-sdk`), `push-proto`, serde_json; existing testcontainers whole-stack test in `push-smoke`; Docker multi-target image; minikube with Calico and the local nginx path.

---

## Authority and boundaries

Read first: `docs\superpowers\specs\2026-10-16-portable-deployment-readiness-design.md`
sections 1, 2, 7, 8 and 9; `docs\deployment-configuration.md`;
`docs\deployment-ingress-verification.md`. Plans 11A (#20) and 11B (#21) are merged.

This plan delivers 11C only. It adds no registry publication, GitHub release or
deployment workflow, no cloud resources, no automatic version bumps and no account
creation for production. The full 10,000-connection run, sizing and performance
acceptance belong to a later consumer deployment; this plan proves the harness
locally and on a small real deployment and never claims capacity.

**Execution notes (recorded after Task 11 ran):** `push11b-local` could not restart
(its node lost the kubelet kubeconfig: `bootstrap-kubelet.conf` missing), so the small
real deployment ran on a new owned profile `push11c-local`; `push11b-local` was left
stopped with its data. `Start-Process -ArgumentList` does not quote array elements, so
the `--target-build` label must contain no spaces there (the harness refused the split
arguments, as intended). Whole-stack servers are spawned through a small `spawn` helper
instead of `async move` blocks, which would capture the shared setup variables.

Start from clean, current `main` containing this reviewed plan:

```powershell
git switch main
git pull --ff-only
git switch -c feature/plan-11c-verification-load
$env:CARGO_INCREMENTAL = '0'
cargo test -p push-smoke --locked
```

Expected: the existing whole-stack smoke test passes (Docker must be running).

Every commit ends with:

```text
Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>
```

## Facts the design depends on (verified in the code)

| Fact | Source | Consequence |
|---|---|---|
| At most 10 active devices per account | `crates\push-auth\src\devices.rs:21` | 10,000 connections need at least 1,000 verified test accounts |
| Login: 60 per IP per 15 min, 10 per account+IP per 15 min, 100 per account per hour | `crates\push-auth\src\limits.rs:88-95` | One login per account; preparing 1,000 accounts from one IP takes about 4.2 h; resumable and paced by `Retry-After` |
| A registration token lasts 5 min and registers several devices | `crates\push-auth\src\service.rs:23`, `devices.rs:110-170` | One login registers all of an account's devices |
| `registration_id` makes registration idempotent; a revoked device's id is spent (404) | `devices.rs:99-157` | Save the id before the request; retry with the same id |
| `POST /v1/auth/login` with `device_id` signs a known device in again | `crates\push-auth\src\api.rs:328-342` | Repair devices whose refresh tokens were lost |
| Refresh is not rate limited, rotates the token, 30 s grace | `limits.rs:14-15`, `devices.rs:25` | Save rotated tokens before use; a lost answer can be retried within 30 s |
| Access tokens last 30 min; `AuthExpiring` 60 s before | `crates\push-auth\src\tokens.rs:17`, `crates\push-gateway\src\server.rs:45-49` | Ramp + duration capped at 25 min; the harness never re-authenticates |
| Gateway frame budget 100 burst, 20/s; idle close after 2.5 heartbeats | `crates\push-gateway\src\limits.rs:290-294`, `server.rs:298-316` | Coalesce `Ack`s (highest seq per conversation); Ping every heartbeat |
| Redeem: 20/user, 60/IP per hour; invites 20/user per hour | `crates\push-gateway\src\limits.rs:93-97` | At most 50 sample pairs |
| Takeover of the same device closes the older connection with 4007 | `crates\push-proto\src\lib.rs` `close` | Every connection uses a distinct device; any 4007 fails the run |

## Interface decisions

| Interface | Contract |
|---|---|
| `--target <url>` | Required, no default, no discovery. `https://host[:port]` only; `http` only for local hosts (`localhost`, `127.0.0.1`, `[::1]`, `*.local`, `*.localhost`, `*.test`). No credentials, path, query or fragment. |
| `--authorized-target <host[:port]>` | Required acknowledgement: must repeat the target's authority exactly (case-insensitive). |
| `--connect-to <ip>` | Optional, DNS-named targets only: dial this IP on the target's port for HTTP and WebSocket, keeping the target host for `Host`, TLS SNI and certificate checks. |
| Accounts file | Operator-supplied JSON Lines `{"email": "...", "password": "..."}` of **verified, dedicated** test accounts; at most 2,000. Parse errors name the line, never echo its content. |
| State file | Private JSON with refresh tokens. Refused inside any Git working tree; written atomically; `0600` on Unix; on Windows the operator supplies a private directory. |
| `prepare` | `--devices-per-account 1-10`, `--sample-pairs 0-50`, `--concurrency 1-16`. Resumable; exit 0 only when complete. |
| `run` | `--connections 1-20000`, `--ramp-per-second 1-500`, `--duration-seconds` (>= 10; ramp + duration <= 25 min), `--message-interval-seconds 1-300` (default 10), `--target-build <label>` (required, recorded), `--report <new file>`, `--no-reconnect`. Exit 0 passed, 1 failed, 130 interrupted, 2 usage/precondition error. |
| Report | JSON: status, failures, workload, counts, close codes, latency summaries (connect, ping RTT, delivery, receipt, reconnect), active peak/min, generator scheduling lag, percentile definition, target and build. Never tokens, passwords or emails. |

The harness does not delete accounts or devices: consumers revoke test devices
through their normal account processes when the test campaign ends.

## File map

| File | Responsibility | Task |
|---|---|---|
| `Cargo.toml` | Workspace member and dependency entry | 1 |
| `crates\push-load\Cargo.toml` | Crate manifest | 1 |
| `crates\push-load\src\lib.rs` | Module list and crate docs | 1 |
| `crates\push-load\src\config.rs` | Target guard and option limits | 1 |
| `crates\push-load\src\accounts.rs` | Accounts file parsing | 2 |
| `crates\push-load\src\state.rs` | Private state file, Git-tree refusal, device selection | 2 |
| `crates\push-load\src\metrics.rs` | Samples, nearest-rank percentiles, counters, message tracking, lag watcher | 3 |
| `crates\push-load\src\report.rs` | Report shape, pass/fail judgement, summary text | 3 |
| `crates\push-load\src\http.rs` | auth-svc client with shared `Retry-After` pacing | 4 |
| `crates\push-load\src\socket.rs` | WebSocket connect, Identify, requests | 4 |
| `crates\push-load\src\prepare.rs` | Resumable preparation | 5 |
| `crates\push-load\src\run.rs` | Measured run | 6 |
| `crates\push-load\src\cli.rs`, `src\main.rs` | Command line | 7 |
| `crates\push-smoke\src\auth.rs`, `src\lib.rs`, `src\bin\push-dev-accounts.rs`, `Cargo.toml` | DEV ONLY verified-account helper | 8 |
| `crates\push-smoke\tests\local.rs` | Whole-stack test refactored into `Stack`, plus the harness correctness test | 8 |
| `deploy\docker\Dockerfile`, `.github\workflows\ci.yml` | `load` image target; contributor CI builds it | 9 |
| `docs\consumer-verification.md` (new) | Build, package, render and verification commands; harness preparation, rollout, interpretation | 10 |
| `README.md`, roadmap, design status | Links and status | 10, 12 |

## Task 1: Crate scaffold and target guards

- [ ] **Step 1: Register the crate.** In the root `Cargo.toml` add `"crates/push-load",`
  to `members` (alphabetically after `push-gateway`) and to `[workspace.dependencies]`:

```toml
push-load = { path = "crates/push-load" }
```

Create `crates\push-load\Cargo.toml` (TLS features exactly as `push-sdk`, whose rustls
provider setup already builds in this lockfile):

```toml
[package]
name = "push-load"
description = "Explicitly targeted, bounded connection and latency verification of a push-messenger deployment."
publish = false
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true

[dependencies]
anyhow.workspace = true
futures-util.workspace = true
push-proto.workspace = true
# HTTPS with rustls, ring and the webpki roots, as push-sdk.
reqwest = { workspace = true, features = ["rustls-tls"] }
serde.workspace = true
serde_json.workspace = true
tokio = { workspace = true, features = ["macros", "net", "rt-multi-thread", "signal", "sync", "time"] }
tokio-tungstenite = { workspace = true, features = ["rustls-tls-webpki-roots"] }
tokio-util.workspace = true
uuid.workspace = true

[lints]
workspace = true
```

Create `crates\push-load\src\lib.rs`:

```rust
//! push-load: explicitly targeted, bounded verification of a push-messenger deployment
//! through its public API only.
//!
//! `prepare` signs in operator-supplied, verified test accounts, registers their devices
//! and makes sample pairs contacts; `run` connects the prepared devices at a fixed rate
//! for a finite duration and reports connection, heartbeat, message, receipt and
//! reconnect measurements. It has no default target, discovers nothing, creates no
//! accounts, never needs auth-svc's signing key or a mailbox, and never prints
//! credentials. Only test a deployment you own or are authorized to test.

pub mod accounts;
pub mod cli;
pub mod config;
pub mod http;
pub mod metrics;
pub mod prepare;
pub mod report;
pub mod run;
pub mod socket;
pub mod state;
```

Until their tasks, create each other module file with only its `//!` line so the crate
builds (`accounts`, `cli`, `http`, `metrics`, `prepare`, `report`, `run`, `socket`,
`state`), e.g. `//! The operator's test accounts.`

- [ ] **Step 2: Write the failing guard tests** at the end of
  `crates\push-load\src\config.rs` (after the code of step 4):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_operator_must_repeat_the_exact_target_authority() {
        assert!(Target::new("https://api.example.com", "api.example.com", None).is_ok());
        assert!(Target::new("https://api.example.com", " API.example.com ", None).is_ok());
        for wrong in ["", "example.com", "api.example.com:443", "other.example.com"] {
            assert!(Target::new("https://api.example.com", wrong, None).is_err(), "{wrong:?}");
        }
        assert!(Target::new("https://api.example.com:8443", "api.example.com", None).is_err());
        assert!(Target::new("https://api.example.com:8443", "api.example.com:8443", None).is_ok());
    }

    #[test]
    fn plain_http_only_for_local_hosts_and_never_a_path_or_credentials() {
        assert!(Target::new("http://push.local", "push.local", None).is_ok());
        assert!(Target::new("http://127.0.0.1:8080", "127.0.0.1:8080", None).is_ok());
        assert!(Target::new("http://api.example.com", "api.example.com", None).is_err());
        for bad in [
            "https://api.example.com/v1",
            "https://api.example.com/?debug=1",
            "https://user:secret@api.example.com",
            "ftp://api.example.com",
            "api.example.com",
        ] {
            assert!(Target::new(bad, "api.example.com", None).is_err(), "{bad}");
        }
    }

    #[test]
    fn urls_and_dialing_follow_the_target() {
        let target =
            Target::new("https://api.example.com", "api.example.com", Some("10.0.0.5")).unwrap();
        assert_eq!(target.api_url("/v1/auth/login"), "https://api.example.com/v1/auth/login");
        assert_eq!(target.socket_url(), "wss://api.example.com/v1/socket");
        assert_eq!(target.dial_address(), "10.0.0.5:443");
        let local = Target::new("http://push.local:18080", "push.local:18080", None).unwrap();
        assert_eq!(local.socket_url(), "ws://push.local:18080/v1/socket");
        assert_eq!(local.dial_address(), "push.local:18080");
        // An IP target needs no override; a malformed override is refused.
        assert!(Target::new("http://127.0.0.1:8080", "127.0.0.1:8080", Some("127.0.0.2")).is_err());
        assert!(Target::new("https://api.example.com", "api.example.com", Some("10.0.0.5:1")).is_err());
    }

    fn run_options() -> RunOptions {
        RunOptions {
            connections: 10_000,
            ramp_per_second: 100,
            duration: Duration::from_secs(20 * 60),
            message_interval: Duration::from_secs(10),
            target_build: "gateway@sha256:0123".to_owned(),
            reconnect: true,
        }
    }

    #[test]
    fn runs_are_bounded() {
        run_options().validate().unwrap();
        let ok = run_options();
        for bad in [
            RunOptions { connections: 0, ..ok.clone() },
            RunOptions { connections: MAX_CONNECTIONS + 1, ..ok.clone() },
            RunOptions { ramp_per_second: 0, ..ok.clone() },
            RunOptions { ramp_per_second: MAX_RAMP_PER_SECOND + 1, ..ok.clone() },
            RunOptions { duration: Duration::from_secs(5), ..ok.clone() },
            // 100 s of ramp plus 24 minutes exceeds 25 minutes.
            RunOptions { duration: Duration::from_secs(24 * 60), ..ok.clone() },
            RunOptions { message_interval: Duration::from_millis(500), ..ok.clone() },
            RunOptions { target_build: String::new(), ..ok.clone() },
            RunOptions { target_build: "two\nlines".to_owned(), ..ok.clone() },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn preparation_is_bounded() {
        let ok = PrepareOptions { devices_per_account: 10, sample_pairs: 50, concurrency: 16 };
        ok.validate().unwrap();
        for bad in [
            PrepareOptions { devices_per_account: 0, ..ok },
            PrepareOptions { devices_per_account: 11, ..ok },
            PrepareOptions { sample_pairs: 51, ..ok },
            PrepareOptions { concurrency: 0, ..ok },
            PrepareOptions { concurrency: 17, ..ok },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
```

- [ ] **Step 3: Run them red.**

```powershell
cargo test -p push-load --lib config --locked
```

Expected: compile errors (`Target`, `RunOptions`, `PrepareOptions` undefined). The
first build adds `push-load` to `Cargo.lock`; drop `--locked` for this one command if
Cargo refuses, then keep it for every later command.

- [ ] **Step 4: Implement `config.rs`** above the tests:

```rust
//! The target and every bound of a preparation or run. Nothing here touches the
//! network: the guards run before any request.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::Url;

/// Most connections one run opens and devices one preparation keeps.
pub const MAX_CONNECTIONS: usize = 20_000;
/// auth-svc's limit of active devices per account (`MAX_ACTIVE_DEVICES`).
pub const MAX_DEVICES_PER_ACCOUNT: usize = 10;
/// Most sender/receiver pairs; the gateway allows 20 redeems per user and 60 per IP an hour.
pub const MAX_SAMPLE_PAIRS: usize = 50;
pub const MAX_RAMP_PER_SECOND: u32 = 500;
pub const MAX_PREPARE_CONCURRENCY: usize = 16;
/// Ramp plus duration. Access tokens last 30 minutes and `AuthExpiring` comes a minute
/// before; the harness refreshes once, before connecting, and never re-authenticates.
pub const MAX_RUN: Duration = Duration::from_secs(25 * 60);
pub const MIN_DURATION: Duration = Duration::from_secs(10);
pub const MIN_MESSAGE_INTERVAL: Duration = Duration::from_secs(1);
pub const MAX_MESSAGE_INTERVAL: Duration = Duration::from_secs(300);
const MAX_BUILD_LABEL: usize = 128;

/// The deployment under test: named explicitly and confirmed by the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    scheme: String,
    host: String,
    port: u16,
    authority: String,
    connect_to: Option<IpAddr>,
}

impl Target {
    /// `target` is `https://host[:port]` (`http` only for a local host); `authorized`
    /// must repeat its authority exactly; `connect_to` dials another IP for a DNS name.
    pub fn new(target: &str, authorized: &str, connect_to: Option<&str>) -> anyhow::Result<Self> {
        let url = Url::parse(target)
            .context("--target must be an absolute URL such as https://api.example.com")?;
        let host = url
            .host_str()
            .context("--target has no host")?
            .to_ascii_lowercase();
        if !url.username().is_empty() || url.password().is_some() {
            bail!("--target must not contain credentials");
        }
        if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
            bail!("--target must be only scheme://host[:port], without a path, query or fragment");
        }
        match url.scheme() {
            "https" => {}
            "http" if is_local(&host) => {}
            "http" => bail!(
                "--target may use http only for a local host (localhost, 127.0.0.1, [::1], *.local, *.localhost or *.test)"
            ),
            other => bail!("--target must use https (http only for a local host), not {other}"),
        }
        let port = url.port_or_known_default().context("--target has no port")?;
        let authority = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.clone(),
        };
        if !authorized.trim().eq_ignore_ascii_case(&authority) {
            bail!(
                "--authorized-target must repeat the target exactly ({authority}), confirming that you own or are authorized to test it"
            );
        }
        let connect_to = match connect_to {
            None => None,
            Some(_) if is_ip(&host) => bail!("--connect-to is only for a target named by DNS"),
            Some(value) => Some(
                value
                    .parse::<IpAddr>()
                    .context("--connect-to must be an IP address such as 10.0.0.5 (the target's port is used)")?,
            ),
        };
        Ok(Self {
            scheme: url.scheme().to_owned(),
            host,
            port,
            authority,
            connect_to,
        })
    }

    /// `host[:port]` as written in the target: what reports name.
    pub fn authority(&self) -> &str {
        &self.authority
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn base_url(&self) -> String {
        format!("{}://{}", self.scheme, self.authority)
    }

    pub fn api_url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url())
    }

    pub fn socket_url(&self) -> String {
        let scheme = if self.scheme == "https" { "wss" } else { "ws" };
        format!("{scheme}://{}/v1/socket", self.authority)
    }

    pub fn connect_to(&self) -> Option<IpAddr> {
        self.connect_to
    }

    /// Where TCP connections go: `--connect-to` on the target's port, or the host itself.
    pub fn dial_address(&self) -> String {
        match self.connect_to {
            Some(IpAddr::V6(ip)) => format!("[{ip}]:{}", self.port),
            Some(ip) => format!("{ip}:{}", self.port),
            None => format!("{}:{}", self.host, self.port),
        }
    }
}

fn is_local(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
        || [".local", ".localhost", ".test"]
            .iter()
            .any(|suffix| host.ends_with(suffix))
}

fn is_ip(host: &str) -> bool {
    host.starts_with('[') || host.parse::<IpAddr>().is_ok()
}

/// The bounds of a preparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrepareOptions {
    pub devices_per_account: usize,
    pub sample_pairs: usize,
    pub concurrency: usize,
}

impl PrepareOptions {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !(1..=MAX_DEVICES_PER_ACCOUNT).contains(&self.devices_per_account) {
            bail!("--devices-per-account must be 1 to {MAX_DEVICES_PER_ACCOUNT}");
        }
        if self.sample_pairs > MAX_SAMPLE_PAIRS {
            bail!("--sample-pairs must be at most {MAX_SAMPLE_PAIRS}");
        }
        if !(1..=MAX_PREPARE_CONCURRENCY).contains(&self.concurrency) {
            bail!("--concurrency must be 1 to {MAX_PREPARE_CONCURRENCY}");
        }
        Ok(())
    }
}

/// The bounds of a measured run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    pub connections: usize,
    pub ramp_per_second: u32,
    pub duration: Duration,
    pub message_interval: Duration,
    /// What the operator deployed (image digests, chart version): recorded in the report.
    pub target_build: String,
    pub reconnect: bool,
}

impl RunOptions {
    pub fn ramp_time(&self) -> Duration {
        Duration::from_secs_f64(self.connections as f64 / f64::from(self.ramp_per_second.max(1)))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !(1..=MAX_CONNECTIONS).contains(&self.connections) {
            bail!("--connections must be 1 to {MAX_CONNECTIONS}");
        }
        if !(1..=MAX_RAMP_PER_SECOND).contains(&self.ramp_per_second) {
            bail!("--ramp-per-second must be 1 to {MAX_RAMP_PER_SECOND}");
        }
        if self.duration < MIN_DURATION {
            bail!("--duration-seconds must be at least {}", MIN_DURATION.as_secs());
        }
        if self.ramp_time() + self.duration > MAX_RUN {
            bail!(
                "the ramp ({:.0} s) plus the duration must stay within {} minutes: access tokens last 30 minutes and push-load does not re-authenticate",
                self.ramp_time().as_secs_f64(),
                MAX_RUN.as_secs() / 60
            );
        }
        if !(MIN_MESSAGE_INTERVAL..=MAX_MESSAGE_INTERVAL).contains(&self.message_interval) {
            bail!("--message-interval-seconds must be 1 to 300");
        }
        let build = self.target_build.trim();
        if build.is_empty()
            || build.len() > MAX_BUILD_LABEL
            || !build.chars().all(|c| c.is_ascii_graphic() || c == ' ')
        {
            bail!("--target-build must be 1 to {MAX_BUILD_LABEL} printable ASCII characters naming what is deployed");
        }
        Ok(())
    }
}
```

- [ ] **Step 5: Run green and commit.**

```powershell
cargo test -p push-load --lib config --locked
git add Cargo.toml Cargo.lock crates\push-load
git commit -m "feat: scaffold push-load with explicit target guards" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

Expected: 5 passed.

## Task 2: Accounts file and private state

- [ ] **Step 1: Write `crates\push-load\src\accounts.rs`** with its tests:

```rust
//! The operator's test accounts: JSON Lines, one `{"email": "...", "password": "..."}`
//! per line. They must be verified, dedicated test accounts: push-load never creates
//! accounts against a deployment.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

pub const MAX_ACCOUNTS: usize = 2_000;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub email: String,
    pub password: String,
}

// Neither the address nor the password belongs in output.
impl std::fmt::Debug for Account {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Account { .. }")
    }
}

/// Parses the accounts; errors name the line, never its content.
pub fn parse(text: &str) -> anyhow::Result<Vec<Account>> {
    let mut accounts = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let number = index + 1;
        // serde_json's message can quote the input, so it is not shown.
        let account: Account = serde_json::from_str(line).map_err(|_| {
            anyhow::anyhow!(r#"accounts line {number}: expected {{"email": "...", "password": "..."}}"#)
        })?;
        if account.email.trim().is_empty() || account.password.is_empty() {
            bail!("accounts line {number}: email and password must not be empty");
        }
        if !seen.insert(account.email.trim().to_lowercase()) {
            bail!("accounts line {number}: the same email appears twice");
        }
        accounts.push(account);
        if accounts.len() > MAX_ACCOUNTS {
            bail!("the accounts file holds more than {MAX_ACCOUNTS} accounts");
        }
    }
    if accounts.is_empty() {
        bail!("the accounts file holds no accounts");
    }
    Ok(accounts)
}

pub fn read(path: &Path) -> anyhow::Result<Vec<Account>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read the accounts file {}", path.display()))?;
    parse(&text)
}

/// The accounts as JSON Lines.
pub fn to_lines(accounts: &[Account]) -> String {
    accounts
        .iter()
        .map(|account| serde_json::to_string(account).expect("an account serializes") + "\n")
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_round_trip_and_blank_lines_are_skipped() {
        let accounts = parse("\n{\"email\":\"a@x.test\",\"password\":\"pw-a\"}\n\n{\"email\":\"b@x.test\",\"password\":\"pw-b\"}\n").unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(parse(&to_lines(&accounts)).unwrap(), accounts);
        assert_eq!(format!("{:?}", accounts[0]), "Account { .. }");
    }

    #[test]
    fn bad_lines_are_named_without_their_content() {
        let error = parse("{\"email\":\"a@x.test\",\"password\":\"hunter2\",\"extra\":1}")
            .unwrap_err()
            .to_string();
        assert!(error.contains("line 1") && !error.contains("hunter2"), "{error}");
        let duplicate = "{\"email\":\"a@x.test\",\"password\":\"1\"}\n{\"email\":\"A@X.test\",\"password\":\"2\"}";
        assert!(parse(duplicate).unwrap_err().to_string().contains("line 2"));
        assert!(parse("{\"email\":\"\",\"password\":\"x\"}").is_err());
        assert!(parse("\n\n").is_err());
    }
}
```

- [ ] **Step 2: Write the failing state tests** at the end of
  `crates\push-load\src\state.rs` (after the code of step 4):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("push-load-state-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ready_device() -> DeviceState {
        DeviceState {
            device_id: Some(Uuid::now_v7()),
            refresh_token: Some(format!("refresh-{}", Uuid::now_v7())),
            ..DeviceState::new()
        }
    }

    #[test]
    fn credential_files_are_refused_inside_a_git_working_tree() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join("repo").join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("repo").join("nested")).unwrap();
        assert!(ensure_outside_git(&dir.join("repo").join("nested").join("state.json")).is_err());
        assert!(write_private(&dir.join("repo").join("state.json"), b"{}").is_err());
        ensure_outside_git(&dir.join("state.json")).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn state_round_trips_atomically_and_redacts_tokens_in_debug() {
        let dir = scratch();
        let path = dir.join("state.json");
        assert_eq!(State::load(&path).unwrap(), None);
        let mut state = State::new("push.test:8080");
        state.accounts.push(AccountState::new("a@x.test"));
        state.accounts[0].devices.push(ready_device());
        state.save(&path).unwrap();
        state.save(&path).unwrap();
        assert_eq!(State::load(&path).unwrap(), Some(state.clone()));
        let token = state.accounts[0].devices[0].refresh_token.clone().unwrap();
        assert!(!format!("{state:?}").contains(&token));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temporary file left");
        std::fs::write(&path, "not json").unwrap();
        assert!(State::load(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ready_devices_put_ready_pairs_first_and_skip_unready_devices() {
        let mut state = State::new("push.test");
        for email in ["a@x.test", "b@x.test", "c@x.test"] {
            state.accounts.push(AccountState::new(email));
        }
        state.accounts[0].devices = vec![ready_device(), ready_device()];
        state.accounts[1].devices = vec![DeviceState { needs_sign_in: true, ..ready_device() }, ready_device()];
        state.accounts[2].devices = vec![DeviceState::new(), DeviceState { retired: true, ..ready_device() }, ready_device()];
        let at = |account, device| DeviceRef { account, device };
        state.pairs.push(Pair { sender: at(2, 2), receiver: at(1, 1), conversation_id: Uuid::now_v7() });
        // A pair with an unready device is left out.
        state.pairs.push(Pair { sender: at(1, 0), receiver: at(0, 1), conversation_id: Uuid::now_v7() });
        assert_eq!(
            state.ready_devices(),
            [at(2, 2), at(1, 1), at(0, 0), at(0, 1)]
        );
    }
}
```

- [ ] **Step 3: Run red.**

```powershell
cargo test -p push-load --lib state --locked
```

Expected: compile errors for the undefined state types.

- [ ] **Step 4: Implement `state.rs`** above the tests:

```rust
//! The prepared devices. The file holds refresh tokens, which are credentials: it is
//! refused inside any Git working tree, written atomically and, on Unix, readable only
//! by its owner. On Windows keep it in a private directory (the directory's ACL decides).

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    /// The target's authority this state was prepared for.
    pub target: String,
    pub accounts: Vec<AccountState>,
    pub pairs: Vec<Pair>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountState {
    pub email: String,
    pub user_id: Option<Uuid>,
    pub devices: Vec<DeviceState>,
    /// Why the last preparation could not use this account.
    pub failure: Option<String>,
}

impl AccountState {
    pub fn new(email: &str) -> Self {
        Self {
            email: email.trim().to_owned(),
            user_id: None,
            devices: Vec::new(),
            failure: None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceState {
    /// Saved before registering, so a retry finds the same device.
    pub registration_id: Uuid,
    pub device_id: Option<Uuid>,
    pub refresh_token: Option<String>,
    /// A run found its refresh token refused: the next preparation signs it in again.
    #[serde(default)]
    pub needs_sign_in: bool,
    /// Revoked or never registrable: kept so indices in pairs stay stable, never used.
    #[serde(default)]
    pub retired: bool,
}

impl std::fmt::Debug for DeviceState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceState")
            .field("registration_id", &self.registration_id)
            .field("device_id", &self.device_id)
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "<redacted>"))
            .field("needs_sign_in", &self.needs_sign_in)
            .field("retired", &self.retired)
            .finish()
    }
}

impl DeviceState {
    pub fn new() -> Self {
        Self {
            registration_id: Uuid::new_v4(),
            device_id: None,
            refresh_token: None,
            needs_sign_in: false,
            retired: false,
        }
    }

    pub fn is_ready(&self) -> bool {
        !self.retired && !self.needs_sign_in && self.device_id.is_some() && self.refresh_token.is_some()
    }
}

impl Default for DeviceState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceRef {
    pub account: usize,
    pub device: usize,
}

/// Two devices of two accounts that are contacts, with their direct conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pair {
    pub sender: DeviceRef,
    pub receiver: DeviceRef,
    pub conversation_id: Uuid,
}

impl State {
    pub fn new(target: &str) -> Self {
        Self {
            version: VERSION,
            target: target.to_owned(),
            accounts: Vec::new(),
            pairs: Vec::new(),
        }
    }

    /// The state at `path`, `None` when there is none yet.
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("read the state file {}", path.display()));
            }
        };
        // serde_json's message can quote tokens, so it is not shown.
        let state: Self = serde_json::from_str(&text)
            .map_err(|_| anyhow::anyhow!("{} is not a push-load state file", path.display()))?;
        if state.version != VERSION {
            bail!("{} has state version {}, not {VERSION}", path.display(), state.version);
        }
        Ok(Some(state))
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).context("serialize the state")?;
        write_private(path, &bytes)
    }

    pub fn device(&self, at: DeviceRef) -> &DeviceState {
        &self.accounts[at.account].devices[at.device]
    }

    pub fn device_mut(&mut self, at: DeviceRef) -> &mut DeviceState {
        &mut self.accounts[at.account].devices[at.device]
    }

    fn is_ready(&self, at: DeviceRef) -> bool {
        self.accounts
            .get(at.account)
            .and_then(|account| account.devices.get(at.device))
            .is_some_and(DeviceState::is_ready)
    }

    pub fn pair_is_ready(&self, pair: &Pair) -> bool {
        self.is_ready(pair.sender) && self.is_ready(pair.receiver)
    }

    /// The ready devices: the ready pairs' first, so that any run of at least twice the
    /// pairs includes them, then by account and device.
    pub fn ready_devices(&self) -> Vec<DeviceRef> {
        let mut seen = HashSet::new();
        let mut ordered = Vec::new();
        let paired = self
            .pairs
            .iter()
            .filter(|pair| self.pair_is_ready(pair))
            .flat_map(|pair| [pair.sender, pair.receiver]);
        let all = self.accounts.iter().enumerate().flat_map(|(account, state)| {
            (0..state.devices.len()).map(move |device| DeviceRef { account, device })
        });
        for at in paired.chain(all) {
            if self.is_ready(at) && seen.insert(at) {
                ordered.push(at);
            }
        }
        ordered
    }
}

/// Refuses a credential file inside a Git working tree, where it could be committed.
pub fn ensure_outside_git(path: &Path) -> anyhow::Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("the current directory")?
            .join(path)
    };
    let mut directory = absolute.parent();
    while let Some(current) = directory {
        if current.join(".git").exists() {
            bail!(
                "{} is inside the Git working tree {}: keep push-load credential files in a private directory outside every repository",
                path.display(),
                current.display()
            );
        }
        directory = current.parent();
    }
    Ok(())
}

/// Writes `bytes` to `path` through a temporary file renamed over it, owner-only on Unix.
pub fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    ensure_outside_git(path)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("write {}", temporary.display()))?;
    }
    std::fs::rename(&temporary, path)
        .with_context(|| format!("replace {}", path.display()))
}
```

- [ ] **Step 5: Run green and commit.**

```powershell
cargo test -p push-load --lib --locked
git add crates\push-load
git commit -m "feat: add push-load accounts and private resumable state" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

Expected: the config, accounts and state tests pass.

## Task 3: Measurements and the pass/fail judgement

- [ ] **Step 1: Write `crates\push-load\src\metrics.rs`** with its tests (pure code; the
  tests come first in the file's test module and fail to compile until the code exists):

```rust
//! What a run measures: latency samples summarized by nearest rank, counters, the
//! sample messages in flight and the load generator's own scheduling lag.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

/// How every percentile in a report is defined.
pub const PERCENTILES: &str = "Nearest rank over every sample of the run, in milliseconds: pN is the smallest sample with at least N% of the samples at or below it; max is the largest sample. Connect latency runs from a connection's first attempt to Ready (including 4006 retries); reconnect latency from losing an open connection to Ready again; delivery from sending a sample message to its first Deliver on a receiving device; receipt from sending to the sender's Receipt covering it.";

/// How often the lag watcher expects to wake up.
pub const LAG_TICK: Duration = Duration::from_millis(100);
/// A p99 scheduling lag above this means the generator, not the target, limited the run.
pub const LAG_LIMIT: Duration = Duration::from_millis(250);

#[derive(Debug, Default)]
pub struct Samples(Mutex<Vec<u32>>);

impl Samples {
    pub fn record(&self, duration: Duration) {
        let millis = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
        self.0.lock().expect("samples lock").push(millis);
    }

    pub fn summary(&self) -> Summary {
        let mut values = self.0.lock().expect("samples lock").clone();
        values.sort_unstable();
        Summary::of(&values)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub count: usize,
    pub p50_ms: Option<u32>,
    pub p90_ms: Option<u32>,
    pub p99_ms: Option<u32>,
    pub max_ms: Option<u32>,
}

impl Summary {
    /// `sorted` ascending.
    pub fn of(sorted: &[u32]) -> Self {
        if sorted.is_empty() {
            return Self::default();
        }
        Self {
            count: sorted.len(),
            p50_ms: Some(nearest_rank(sorted, 50)),
            p90_ms: Some(nearest_rank(sorted, 90)),
            p99_ms: Some(nearest_rank(sorted, 99)),
            max_ms: sorted.last().copied(),
        }
    }
}

/// The smallest value with at least `percentile`% of `sorted` (ascending, non-empty) at or below it.
pub fn nearest_rank(sorted: &[u32], percentile: usize) -> u32 {
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

pub fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

pub fn read(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

#[derive(Debug, Default)]
pub struct Counters {
    pub ready_once: AtomicU64,
    pub attempt_errors: AtomicU64,
    pub connect_failures: AtomicU64,
    pub reconnect_failures: AtomicU64,
    pub auth_failures: AtomicU64,
    pub takeovers: AtomicU64,
    pub rate_limited: AtomicU64,
    pub pong_timeouts: AtomicU64,
    pub protocol_failures: AtomicU64,
    pub disconnects: AtomicU64,
    pub reconnects: AtomicU64,
    pub pings: AtomicU64,
    pub pongs: AtomicU64,
    pub auth_expiring: AtomicU64,
    pub error_frames: AtomicU64,
    active: AtomicI64,
    peak: AtomicI64,
}

impl Counters {
    pub fn opened(&self) {
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
    }

    pub fn closed(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn active(&self) -> i64 {
        self.active.load(Ordering::SeqCst)
    }

    pub fn peak(&self) -> i64 {
        self.peak.load(Ordering::SeqCst)
    }
}

/// WebSocket close codes and how often each came.
#[derive(Debug, Default)]
pub struct CloseCodes(Mutex<BTreeMap<u16, u64>>);

impl CloseCodes {
    pub fn record(&self, code: u16) {
        *self.0.lock().expect("close codes lock").entry(code).or_default() += 1;
    }

    pub fn snapshot(&self) -> BTreeMap<String, u64> {
        self.0
            .lock()
            .expect("close codes lock")
            .iter()
            .map(|(code, count)| (code.to_string(), *count))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct MessageCounts {
    pub sent: u64,
    /// Answered by `SendAck`: durably stored.
    pub stored: u64,
    pub delivered: u64,
    pub receipted: u64,
    /// Stored but never delivered to a receiving device by the end of the run.
    pub lost: u64,
    /// Sent without a `SendAck` (the sender's connection ended first).
    pub unconfirmed: u64,
}

#[derive(Debug)]
struct Pending {
    sent_at: Instant,
    conversation: Vec<u8>,
    seq: Option<i64>,
    delivered: bool,
    receipted: bool,
}

/// The sample messages of a run, keyed by `client_msg_id`.
#[derive(Debug, Default)]
pub struct Messages {
    pending: Mutex<HashMap<Vec<u8>, Pending>>,
    pub delivery: Samples,
    pub receipt: Samples,
}

impl Messages {
    pub fn sent(&self, client_msg_id: Vec<u8>, conversation: Vec<u8>) {
        let pending = Pending {
            sent_at: Instant::now(),
            conversation,
            seq: None,
            delivered: false,
            receipted: false,
        };
        self.pending.lock().expect("messages lock").insert(client_msg_id, pending);
    }

    pub fn stored(&self, client_msg_id: &[u8], seq: i64) {
        if let Some(message) = self.pending.lock().expect("messages lock").get_mut(client_msg_id) {
            message.seq = Some(seq);
        }
    }

    /// A receiving device got it; only the first delivery counts.
    pub fn delivered(&self, client_msg_id: &[u8]) {
        if let Some(message) = self.pending.lock().expect("messages lock").get_mut(client_msg_id) {
            if !message.delivered {
                message.delivered = true;
                self.delivery.record(message.sent_at.elapsed());
            }
        }
    }

    /// The sender heard that the conversation is delivered up to `delivered_seq`.
    pub fn receipted(&self, conversation: &[u8], delivered_seq: i64) {
        for message in self.pending.lock().expect("messages lock").values_mut() {
            if !message.receipted
                && message.conversation == conversation
                && message.seq.is_some_and(|seq| seq <= delivered_seq)
            {
                message.receipted = true;
                self.receipt.record(message.sent_at.elapsed());
            }
        }
    }

    pub fn counts(&self) -> MessageCounts {
        let pending = self.pending.lock().expect("messages lock");
        let mut counts = MessageCounts::default();
        for message in pending.values() {
            counts.sent += 1;
            counts.stored += u64::from(message.seq.is_some());
            counts.delivered += u64::from(message.delivered);
            counts.receipted += u64::from(message.receipted);
            counts.lost += u64::from(message.seq.is_some() && !message.delivered);
            counts.unconfirmed += u64::from(message.seq.is_none());
        }
        counts
    }
}

/// Records how late a timer that should fire every [`LAG_TICK`] actually fires: a busy
/// generator falls behind and measures itself instead of the target.
pub async fn watch_lag(samples: Arc<Samples>, cancel: CancellationToken) {
    let mut expected = tokio::time::Instant::now() + LAG_TICK;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep_until(expected) => {}
        }
        let now = tokio::time::Instant::now();
        samples.record(now.saturating_duration_since(expected));
        expected = now + LAG_TICK;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_nearest_rank() {
        let values: Vec<u32> = (1..=100).collect();
        let summary = Summary::of(&values);
        assert_eq!(
            (summary.count, summary.p50_ms, summary.p90_ms, summary.p99_ms, summary.max_ms),
            (100, Some(50), Some(90), Some(99), Some(100))
        );
        assert_eq!(nearest_rank(&[7], 99), 7);
        assert_eq!(nearest_rank(&[1, 2, 3], 50), 2);
        assert_eq!(Summary::of(&[]), Summary::default());
    }

    #[test]
    fn messages_count_stored_delivered_receipted_and_lost() {
        let messages = Messages::default();
        for id in [1u8, 2, 3, 4] {
            messages.sent(vec![id], b"conversation".to_vec());
        }
        messages.stored(&[1], 10);
        messages.stored(&[2], 11);
        messages.stored(&[3], 12);
        messages.delivered(&[1]);
        messages.delivered(&[1]);
        messages.delivered(&[2]);
        messages.delivered(&[9]);
        messages.receipted(b"conversation", 11);
        messages.receipted(b"other", 99);
        assert_eq!(
            messages.counts(),
            MessageCounts { sent: 4, stored: 3, delivered: 2, receipted: 2, lost: 1, unconfirmed: 1 }
        );
        assert_eq!(messages.delivery.summary().count, 2);
        assert_eq!(messages.receipt.summary().count, 2);
    }

    #[test]
    fn the_peak_follows_open_connections() {
        let counters = Counters::default();
        counters.opened();
        counters.opened();
        counters.closed();
        counters.opened();
        assert_eq!((counters.active(), counters.peak()), (2, 2));
    }
}
```

- [ ] **Step 2: Write the failing judgement tests** at the end of
  `crates\push-load\src\report.rs` (after the code of step 4):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MessageCounts, Summary};

    fn passing() -> Report {
        let lag = Summary { count: 100, p50_ms: Some(1), p90_ms: Some(2), p99_ms: Some(5), max_ms: Some(9) };
        Report {
            tool: "push-load".to_owned(),
            tool_version: "0.1.0".to_owned(),
            status: Status::Failed,
            failures: Vec::new(),
            target: "https://api.example.com".to_owned(),
            target_build: "test".to_owned(),
            started_unix_s: 0,
            elapsed_s: 60.0,
            workload: Workload {
                connections: 4,
                ramp_per_second: 4,
                ramp_s: 1.0,
                duration_s: 30,
                heartbeat_ms: Some(30_000),
                sample_pairs: 1,
                message_interval_s: 5,
                reconnect: true,
            },
            connections: Connections { ready_once: 4, peak_active: 4, ..Connections::default() },
            close_codes: BTreeMap::new(),
            ready_refusals: BTreeMap::new(),
            messages: MessageCounts { sent: 5, stored: 5, delivered: 5, receipted: 5, ..MessageCounts::default() },
            latency: Latency::default(),
            generator: Generator { scheduling_lag: lag, lag_limit_ms: 250, saturated: false },
            percentiles: String::new(),
        }
    }

    fn judged(report: &Report, interrupted: bool) -> (Status, Vec<String>) {
        judge(report, interrupted)
    }

    #[test]
    fn a_clean_run_passes_and_rollout_closes_are_tolerated() {
        let mut report = passing();
        assert_eq!(judged(&report, false), (Status::Passed, Vec::new()));
        report.close_codes.insert("4005".to_owned(), 4);
        report.close_codes.insert("1006".to_owned(), 1);
        report.connections.reconnects = 5;
        assert_eq!(judged(&report, false).0, Status::Passed);
    }

    #[test]
    fn every_criterion_fails_the_run() {
        let cases: Vec<(&str, Box<dyn Fn(&mut Report)>)> = vec![
            ("never became ready", Box::new(|r| r.connections.ready_once = 3)),
            ("open at once", Box::new(|r| r.connections.peak_active = 3)),
            ("failed to connect", Box::new(|r| r.connections.connect_failures = 1)),
            ("failed to reconnect", Box::new(|r| r.connections.reconnect_failures = 1)),
            ("authentication", Box::new(|r| r.connections.auth_failures = 1)),
            ("taken over", Box::new(|r| r.connections.takeovers = 1)),
            ("rate limited", Box::new(|r| r.connections.rate_limited = 1)),
            ("no Pong", Box::new(|r| r.connections.pong_timeouts = 1)),
            ("decoded", Box::new(|r| r.connections.protocol_failures = 1)),
            ("unexpected close codes: 1008x2", Box::new(|r| { r.close_codes.insert("1008".to_owned(), 2); })),
            ("never delivered", Box::new(|r| r.messages.lost = 1)),
            ("no sample messages", Box::new(|r| r.messages = MessageCounts::default())),
            ("saturated", Box::new(|r| r.generator.saturated = true)),
        ];
        for (expected, change) in cases {
            let mut report = passing();
            change(&mut report);
            let (status, failures) = judged(&report, false);
            assert_eq!(status, Status::Failed, "{expected}");
            assert!(failures.iter().any(|failure| failure.contains(expected)), "{expected}: {failures:?}");
        }
    }

    #[test]
    fn an_interrupted_run_never_passes() {
        let (status, failures) = judged(&passing(), true);
        assert_eq!(status, Status::Interrupted);
        assert!(failures[0].contains("interrupted"));
    }

    #[test]
    fn the_summary_names_status_target_and_build() {
        let mut report = passing();
        let (status, failures) = judged(&report, false);
        report.status = status;
        report.failures = failures;
        let text = summary_text(&report);
        assert!(text.contains("PASSED") && text.contains("https://api.example.com") && text.contains("test"), "{text}");
    }
}
```

- [ ] **Step 3: Run red.**

```powershell
cargo test -p push-load --lib report --locked
```

Expected: compile errors for the undefined report types.

- [ ] **Step 4: Implement `report.rs`** above the tests:

```rust
//! The report of a run and its judgement. `passed` only when every criterion holds; a
//! failed, incomplete or interrupted run is never reported as a success. Reports name
//! the target and build but never hold tokens, passwords or email addresses.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::metrics::{MessageCounts, Summary};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Passed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub tool: String,
    pub tool_version: String,
    pub status: Status,
    pub failures: Vec<String>,
    pub target: String,
    pub target_build: String,
    pub started_unix_s: u64,
    pub elapsed_s: f64,
    pub workload: Workload,
    pub connections: Connections,
    /// Close codes of connections that had reached Ready.
    pub close_codes: BTreeMap<String, u64>,
    /// Close codes answering Identify before Ready (4006 while a device is relayed).
    pub ready_refusals: BTreeMap<String, u64>,
    pub messages: MessageCounts,
    pub latency: Latency,
    pub generator: Generator,
    pub percentiles: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Workload {
    pub connections: usize,
    pub ramp_per_second: u32,
    pub ramp_s: f64,
    pub duration_s: u64,
    pub heartbeat_ms: Option<u64>,
    pub sample_pairs: usize,
    pub message_interval_s: u64,
    pub reconnect: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Connections {
    pub ready_once: u64,
    pub attempt_errors: u64,
    pub connect_failures: u64,
    pub reconnect_failures: u64,
    pub auth_failures: u64,
    pub takeovers: u64,
    pub rate_limited: u64,
    pub pong_timeouts: u64,
    pub protocol_failures: u64,
    pub disconnects: u64,
    pub reconnects: u64,
    pub pings: u64,
    pub pongs: u64,
    pub auth_expiring: u64,
    pub error_frames: u64,
    pub peak_active: i64,
    pub min_active_after_ramp: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Latency {
    pub connect: Summary,
    pub ping_rtt: Summary,
    pub delivery: Summary,
    pub receipt: Summary,
    pub reconnect: Summary,
}

#[derive(Debug, Clone, Serialize)]
pub struct Generator {
    pub scheduling_lag: Summary,
    pub lag_limit_ms: u64,
    pub saturated: bool,
}

/// Closes a deliberate gateway rollout or an ordinary connection drop causes; the
/// connection reconnects and the run may still pass.
pub const EXPECTED_CLOSES: [u16; 4] = [1000, 1001, 1006, 4005];
/// Closes counted under their own failure (authentication, rate limit, takeover).
const COUNTED_CLOSES: [u16; 5] = [4001, 4002, 4003, 4004, 4007];

/// The status and the reasons it is not `Passed`.
pub fn judge(report: &Report, interrupted: bool) -> (Status, Vec<String>) {
    let mut failures = Vec::new();
    let connections = &report.connections;
    let requested = report.workload.connections as u64;
    if connections.ready_once < requested {
        failures.push(format!(
            "{} of {requested} connections never became ready",
            requested - connections.ready_once
        ));
    }
    if u64::try_from(connections.peak_active).unwrap_or(0) < requested {
        failures.push(format!(
            "at most {} of {requested} connections were open at once",
            connections.peak_active
        ));
    }
    for (count, what) in [
        (connections.connect_failures, "connections failed to connect within 60 s"),
        (connections.reconnect_failures, "connections failed to reconnect within 60 s"),
        (connections.auth_failures, "devices failed authentication (refresh refused, or 4001, 4002 or 4003)"),
        (connections.takeovers, "connections were taken over by another connection of the same device (4007)"),
        (connections.rate_limited, "connections were rate limited (4004 or RATE_LIMITED)"),
        (connections.pong_timeouts, "Pings got no Pong within 10 s"),
        (connections.protocol_failures, "frames from the gateway could not be decoded"),
    ] {
        if count > 0 {
            failures.push(format!("{count} {what}"));
        }
    }
    let unexpected: Vec<String> = report
        .close_codes
        .iter()
        .filter(|(code, _)| match code.parse::<u16>() {
            Ok(code) => !EXPECTED_CLOSES.contains(&code) && !COUNTED_CLOSES.contains(&code),
            Err(_) => true,
        })
        .map(|(code, count)| format!("{code}x{count}"))
        .collect();
    if !unexpected.is_empty() {
        failures.push(format!("unexpected close codes: {}", unexpected.join(", ")));
    }
    let messages = &report.messages;
    if report.workload.sample_pairs > 0 && messages.sent == 0 {
        failures.push("no sample messages were sent".to_owned());
    }
    if messages.lost > 0 {
        failures.push(format!("{} stored sample messages were never delivered", messages.lost));
    }
    if report.generator.saturated {
        failures.push(format!(
            "the load generator was saturated (p99 scheduling lag {} ms > {} ms): the results do not describe the target",
            report.generator.scheduling_lag.p99_ms.unwrap_or(0),
            report.generator.lag_limit_ms
        ));
    }
    let status = if interrupted {
        failures.insert(0, "the run was interrupted before its duration ended".to_owned());
        Status::Interrupted
    } else if failures.is_empty() {
        Status::Passed
    } else {
        Status::Failed
    };
    (status, failures)
}

fn pair(summary: &Summary) -> String {
    match (summary.p50_ms, summary.p99_ms) {
        (Some(p50), Some(p99)) => format!("{p50}/{p99}"),
        _ => "-".to_owned(),
    }
}

/// A short human summary; the JSON report holds everything.
pub fn summary_text(report: &Report) -> String {
    let status = match report.status {
        Status::Passed => "PASSED",
        Status::Failed => "FAILED",
        Status::Interrupted => "INTERRUPTED",
    };
    let c = &report.connections;
    let l = &report.latency;
    let m = &report.messages;
    let mut text = format!(
        "push-load: {status} against {} (build {})\n",
        report.target, report.target_build
    );
    let _ = writeln!(
        text,
        "  connections: {}/{} ready, peak {} open, {} disconnects, {} reconnects",
        c.ready_once, report.workload.connections, c.peak_active, c.disconnects, c.reconnects
    );
    let _ = writeln!(
        text,
        "  latency p50/p99 ms: connect {}, ping {}, delivery {}, receipt {}, reconnect {}",
        pair(&l.connect), pair(&l.ping_rtt), pair(&l.delivery), pair(&l.receipt), pair(&l.reconnect)
    );
    let _ = writeln!(
        text,
        "  messages: sent {}, stored {}, delivered {}, receipted {}, lost {}",
        m.sent, m.stored, m.delivered, m.receipted, m.lost
    );
    let _ = writeln!(
        text,
        "  generator: scheduling lag p99 {} ms (limit {} ms)",
        report.generator.scheduling_lag.p99_ms.unwrap_or(0),
        report.generator.lag_limit_ms
    );
    for failure in &report.failures {
        let _ = writeln!(text, "  failure: {failure}");
    }
    text
}
```

- [ ] **Step 5: Run green and commit.**

```powershell
cargo test -p push-load --lib --locked
git add crates\push-load
git commit -m "feat: measure and judge push-load runs" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 4: auth-svc client and WebSocket primitives

- [ ] **Step 1: Write `crates\push-load\src\http.rs`.** Its only pure logic,
  `retry_delay`, is unit tested here; the requests are exercised against the real
  services in Task 8.

```rust
//! auth-svc's public API as push-load uses it: sign-in, device registration and
//! refresh. Errors never carry URLs, bodies or tokens. A 429 or 503 pauses every worker
//! of the process for its `Retry-After`, so the harness respects the target's limits.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

use crate::config::Target;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(900);
const TRANSPORT_ATTEMPTS: u32 = 5;
const TRANSPORT_PAUSE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// 429 or 503: the same request may succeed after this long.
    RetryAfter(Duration),
    /// Any other unexpected status, with auth-svc's stable error code.
    Rejected { status: u16, code: String },
    /// The request did not complete.
    Transport(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RetryAfter(delay) => write!(formatter, "retry after {} s", delay.as_secs()),
            Self::Rejected { status, code } => write!(formatter, "{code} (HTTP {status})"),
            Self::Transport(error) => write!(formatter, "the request failed: {error}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// One device's sign-in, as auth-svc answers it.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct DeviceTokens {
    pub user_id: Uuid,
    pub device_id: Uuid,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in_s: u64,
}

impl std::fmt::Debug for DeviceTokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceTokens")
            .field("user_id", &self.user_id)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// The `Retry-After` of a refusal, between 1 s and 15 minutes.
pub fn retry_delay(header: Option<&str>) -> Duration {
    header
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(Duration::from_secs(1), Duration::from_secs)
        .clamp(Duration::from_secs(1), MAX_RETRY_AFTER)
}

/// When the next request may go: shared by every worker of the process.
#[derive(Debug, Clone, Default)]
pub struct Pacer(Arc<Mutex<Option<Instant>>>);

impl Pacer {
    pub async fn wait(&self) {
        let until = *self.0.lock().await;
        if let Some(until) = until {
            tokio::time::sleep_until(until).await;
        }
    }

    pub async fn defer(&self, delay: Duration) {
        let until = Instant::now() + delay;
        let mut next = self.0.lock().await;
        if next.is_none_or(|current| current < until) {
            *next = Some(until);
            eprintln!("push-load: the target asked to slow down; pausing {} s", delay.as_secs());
        }
    }
}

#[derive(Clone)]
pub struct AuthApi {
    http: Client,
    target: Target,
    pacer: Pacer,
}

impl AuthApi {
    pub fn new(target: &Target) -> anyhow::Result<Self> {
        let mut builder = Client::builder().timeout(REQUEST_TIMEOUT).no_proxy();
        if let Some(ip) = target.connect_to() {
            builder = builder.resolve(target.host(), std::net::SocketAddr::new(ip, target.port()));
        }
        Ok(Self {
            http: builder.build().context("build the HTTP client")?,
            target: target.clone(),
            pacer: Pacer::default(),
        })
    }

    async fn post_once(
        &self,
        path: &str,
        body: &Value,
        bearer: Option<&str>,
        expected: StatusCode,
    ) -> Result<Value, ApiError> {
        let mut request = self.http.post(self.target.api_url(path)).json(body);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| ApiError::Transport(error.without_url().to_string()))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let answer: Value = response.json().await.unwrap_or(Value::Null);
        if status == expected {
            return Ok(answer);
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            return Err(ApiError::RetryAfter(retry_delay(retry_after.as_deref())));
        }
        let code = answer["error"]["code"].as_str().unwrap_or("unknown").to_owned();
        Err(ApiError::Rejected { status: status.as_u16(), code })
    }

    /// Waits out rate limits (pausing every worker) and retries up to five transport
    /// failures; every request here is safe to repeat (registration is idempotent by
    /// `registration_id`, a repeated refresh falls in the 30 s grace).
    async fn post(
        &self,
        path: &str,
        body: Value,
        bearer: Option<&str>,
        expected: StatusCode,
    ) -> Result<Value, ApiError> {
        let mut failures = 0;
        loop {
            self.pacer.wait().await;
            match self.post_once(path, &body, bearer, expected).await {
                Err(ApiError::RetryAfter(delay)) => self.pacer.defer(delay).await,
                Err(ApiError::Transport(error)) => {
                    failures += 1;
                    if failures >= TRANSPORT_ATTEMPTS {
                        return Err(ApiError::Transport(error));
                    }
                    tokio::time::sleep(TRANSPORT_PAUSE).await;
                }
                other => return other,
            }
        }
    }

    /// A registration token for the account.
    pub async fn login(&self, email: &str, password: &str) -> Result<String, ApiError> {
        let answer = self
            .post("/v1/auth/login", json!({"email": email, "password": password}), None, StatusCode::OK)
            .await?;
        answer["registration_token"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(malformed)
    }

    /// Signs a known device in again (its tokens were lost or refused).
    pub async fn login_device(&self, email: &str, password: &str, device_id: Uuid) -> Result<DeviceTokens, ApiError> {
        let body = json!({"email": email, "password": password, "device_id": device_id});
        tokens(self.post("/v1/auth/login", body, None, StatusCode::OK).await?)
    }

    pub async fn register_device(
        &self,
        registration_token: &str,
        registration_id: Uuid,
        name: &str,
    ) -> Result<DeviceTokens, ApiError> {
        let body = json!({"name": name, "platform": "other", "registration_id": registration_id});
        tokens(self.post("/v1/devices", body, Some(registration_token), StatusCode::CREATED).await?)
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<DeviceTokens, ApiError> {
        let body = json!({"refresh_token": refresh_token});
        tokens(self.post("/v1/auth/refresh", body, None, StatusCode::OK).await?)
    }
}

fn malformed() -> ApiError {
    ApiError::Rejected { status: 200, code: "malformed_answer".to_owned() }
}

fn tokens(answer: Value) -> Result<DeviceTokens, ApiError> {
    serde_json::from_value(answer).map_err(|_| malformed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_is_bounded() {
        assert_eq!(retry_delay(Some("42")), Duration::from_secs(42));
        assert_eq!(retry_delay(Some("0")), Duration::from_secs(1));
        assert_eq!(retry_delay(Some("99999")), Duration::from_secs(900));
        assert_eq!(retry_delay(Some("soon")), Duration::from_secs(1));
        assert_eq!(retry_delay(None), Duration::from_secs(1));
    }
}
```

- [ ] **Step 2: Write `crates\push-load\src\socket.rs`.**

```rust
//! The gateway's WebSocket as push-load holds it: one TCP connection per device (to the
//! target, or `--connect-to` on its port), Identify with the access token, then frames.

use std::time::Duration;

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use push_proto::v1::frame::Body;
use push_proto::v1::{self, Identify, request, response};
use push_proto::{decode_frame, encode_frame, id};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, client_async_tls};
use uuid::Uuid;

use crate::config::Target;

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// No close frame: the connection dropped (RFC 6455's 1006).
pub const ABNORMAL_CLOSE: u16 = 1006;
/// A close frame without a code (RFC 6455's 1005).
pub const NO_STATUS: u16 = 1005;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ready {
    pub user: Uuid,
    pub heartbeat: Duration,
}

/// What became of an Identify.
pub enum Opened {
    Ready(Box<Socket>, Ready),
    /// The gateway closed the connection with this code before Ready.
    Closed(u16),
}

pub fn close_code(frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>) -> u16 {
    frame.map_or(NO_STATUS, |frame| frame.code.into())
}

async fn connect(target: &Target) -> anyhow::Result<Socket> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target.dial_address()))
        .await
        .map_err(|_| anyhow::anyhow!("the TCP connection timed out"))?
        .context("the TCP connection failed")?;
    stream.set_nodelay(true).context("set TCP_NODELAY")?;
    let request = target
        .socket_url()
        .into_client_request()
        .context("the socket URL")?;
    let (socket, _) = tokio::time::timeout(CONNECT_TIMEOUT, client_async_tls(request, stream))
        .await
        .map_err(|_| anyhow::anyhow!("the WebSocket handshake timed out"))?
        .context("the WebSocket handshake failed")?;
    Ok(socket)
}

/// Connects and identifies with `access_token`.
pub async fn open(target: &Target, access_token: &str) -> anyhow::Result<Opened> {
    let mut socket = connect(target).await?;
    let identify = Body::Identify(Identify { token: access_token.to_owned() });
    send(&mut socket, identify).await?;
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    loop {
        let message = tokio::time::timeout_at(deadline, socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("no answer to Identify within {ANSWER_TIMEOUT:?}"))?;
        match message {
            Some(Ok(Message::Binary(bytes))) => match decode_frame(&bytes).context("decode a frame")? {
                Body::Ready(ready) => {
                    if ready.heartbeat_ms == 0 {
                        bail!("Ready announced no heartbeat");
                    }
                    let user = id(&ready.user_id).map_err(|_| anyhow::anyhow!("Ready names no valid user"))?;
                    let heartbeat = Duration::from_millis(u64::from(ready.heartbeat_ms));
                    return Ok(Opened::Ready(Box::new(socket), Ready { user, heartbeat }));
                }
                _ => bail!("the gateway answered Identify with another frame"),
            },
            Some(Ok(Message::Close(frame))) => return Ok(Opened::Closed(close_code(frame))),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return Ok(Opened::Closed(ABNORMAL_CLOSE)),
        }
    }
}

pub async fn send(socket: &mut Socket, body: Body) -> anyhow::Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, socket.send(Message::Binary(encode_frame(body).into())))
        .await
        .map_err(|_| anyhow::anyhow!("sending a frame timed out"))?
        .context("sending a frame failed")
}

pub async fn close(mut socket: Socket) {
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.close(None)).await;
}

/// Sends a request and waits for its answer, ignoring other frames: for preparation,
/// never for the measured run.
pub async fn ask(
    socket: &mut Socket,
    request_id: u64,
    body: request::Body,
) -> anyhow::Result<Result<response::Body, v1::Error>> {
    let request = v1::Request { request_id, body: Some(body) };
    send(socket, Body::Request(request)).await?;
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    loop {
        let message = tokio::time::timeout_at(deadline, socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("no answer to a request within {ANSWER_TIMEOUT:?}"))?;
        match message {
            Some(Ok(Message::Binary(bytes))) => match decode_frame(&bytes).context("decode a frame")? {
                Body::Response(answer) if answer.request_id == request_id => {
                    return Ok(Ok(answer.body.context("an empty answer")?));
                }
                Body::Error(error) if error.request_id == Some(request_id) => return Ok(Err(error)),
                _ => {}
            },
            Some(Ok(Message::Close(frame))) => {
                bail!("the gateway closed the connection with {}", close_code(frame));
            }
            Some(Ok(_)) => {}
            Some(Err(_)) | None => bail!("the connection dropped"),
        }
    }
}
```

- [ ] **Step 3: Build, lint and commit.**

```powershell
cargo test -p push-load --lib --locked
cargo clippy -p push-load --all-targets --locked -- -D warnings
git add crates\push-load
git commit -m "feat: add push-load auth client and socket primitives" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

Expected: the tests pass and Clippy is clean. If `client_async_tls` is not exported
with these features, use `tokio_tungstenite::client_async_tls_with_config(request,
stream, None, None)`; keep the behaviour (TLS for `wss`, plain for `ws`).

## Task 5: Resumable preparation

- [ ] **Step 1: Write `crates\push-load\src\prepare.rs`.** It is exercised end to end
  against the real services in Task 8 (fresh, resumed, failed account, repair).

```rust
//! Preparation, through the public API only: signs in the operator's accounts, registers
//! their devices and makes sample pairs contacts with a direct conversation. Resumable:
//! the state is saved after every change and before every request that could create
//! something, so an interrupted preparation continues where it stopped.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use push_proto::v1::{CreateInvite, OpenDirect, RedeemInvite, request, response};
use push_proto::{id, id_bytes};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio::time::Instant;
use uuid::Uuid;

use crate::accounts::Account;
use crate::config::{MAX_CONNECTIONS, PrepareOptions, Target};
use crate::http::{ApiError, AuthApi, DeviceTokens};
use crate::socket::{self, Opened, Ready, Socket};
use crate::state::{AccountState, DeviceRef, DeviceState, Pair, State};

/// How long a newly registered device may take to reach the gateway (the worker relays it).
const READY_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareSummary {
    pub accounts: usize,
    pub devices_ready: usize,
    pub devices_wanted: usize,
    pub pairs_ready: usize,
    pub pairs_wanted: usize,
    /// `accounts line N: reason`, in the accounts file's order.
    pub failures: Vec<String>,
}

impl PrepareSummary {
    pub fn complete(&self) -> bool {
        self.failures.is_empty()
            && self.devices_ready >= self.devices_wanted
            && self.pairs_ready >= self.pairs_wanted
    }
}

/// The state, shared by the workers; every change is saved at once.
#[derive(Clone)]
struct Shared {
    state: Arc<Mutex<State>>,
    path: Arc<Path>,
}

impl Shared {
    async fn update<T>(&self, change: impl FnOnce(&mut State) -> T) -> anyhow::Result<T> {
        let mut state = self.state.lock().await;
        let result = change(&mut state);
        state.save(&self.path)?;
        Ok(result)
    }

    async fn read<T>(&self, look: impl FnOnce(&State) -> T) -> T {
        look(&*self.state.lock().await)
    }
}

/// Why an account could not be prepared, or why preparation must stop.
enum Failure {
    Account(String),
    Fatal(anyhow::Error),
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self::Fatal(error)
    }
}

fn classify(error: ApiError) -> Failure {
    match error {
        ApiError::Rejected { status: 401, .. } => Failure::Account("invalid_credentials: wrong email or password".to_owned()),
        ApiError::Rejected { status: 403, .. } => Failure::Account("email_not_verified: verify the account before using it".to_owned()),
        ApiError::Rejected { status: 409, .. } => Failure::Account(
            "too_many_devices: the account has other active devices; use dedicated test accounts".to_owned(),
        ),
        ApiError::Rejected { status, code } => Failure::Account(format!("{code} (HTTP {status})")),
        other => Failure::Fatal(anyhow::anyhow!("the target could not be used: {other}")),
    }
}

pub async fn prepare(
    target: &Target,
    accounts: &[Account],
    state_path: &Path,
    options: &PrepareOptions,
) -> anyhow::Result<PrepareSummary> {
    options.validate()?;
    crate::state::ensure_outside_git(state_path)?;
    if options.sample_pairs * 2 > accounts.len() {
        bail!("--sample-pairs {} needs at least {} accounts", options.sample_pairs, options.sample_pairs * 2);
    }
    if accounts.len() * options.devices_per_account > MAX_CONNECTIONS {
        bail!("at most {MAX_CONNECTIONS} devices: use fewer accounts or devices per account");
    }
    let mut state = State::load(state_path)?.unwrap_or_else(|| State::new(target.authority()));
    if state.target != target.authority() {
        bail!("{} was prepared for {}, not {}", state_path.display(), state.target, target.authority());
    }
    let mut indices = Vec::with_capacity(accounts.len());
    for account in accounts {
        let email = account.email.trim();
        let index = match state.accounts.iter().position(|known| known.email.eq_ignore_ascii_case(email)) {
            Some(index) => index,
            None => {
                state.accounts.push(AccountState::new(email));
                state.accounts.len() - 1
            }
        };
        state.accounts[index].failure = None;
        indices.push(index);
    }
    state.save(state_path)?;

    let api = AuthApi::new(target)?;
    let shared = Shared {
        state: Arc::new(Mutex::new(state)),
        path: Arc::from(state_path),
    };
    let limit = Arc::new(Semaphore::new(options.concurrency));
    let mut workers = JoinSet::new();
    for (account, index) in accounts.iter().cloned().zip(indices.iter().copied()) {
        let (api, shared, limit) = (api.clone(), shared.clone(), limit.clone());
        let wanted = options.devices_per_account;
        workers.spawn(async move {
            let _permit = limit.acquire_owned().await.expect("the semaphore stays open");
            match prepare_account(&api, &shared, index, &account, wanted).await {
                Ok(()) => Ok(()),
                Err(Failure::Account(reason)) => {
                    shared.update(|state| state.accounts[index].failure = Some(reason)).await
                }
                Err(Failure::Fatal(error)) => Err(error),
            }
        });
    }
    while let Some(joined) = workers.join_next().await {
        joined.context("a preparation worker panicked")??;
    }
    let pairs_ready = prepare_pairs(&api, target, &shared, &indices, options.sample_pairs).await?;
    let state = shared.state.lock().await;
    let listed = || indices.iter().map(|&index| &state.accounts[index]);
    Ok(PrepareSummary {
        accounts: indices.len(),
        devices_ready: listed()
            .map(|account| account.devices.iter().filter(|device| device.is_ready()).count().min(options.devices_per_account))
            .sum(),
        devices_wanted: indices.len() * options.devices_per_account,
        pairs_ready,
        pairs_wanted: options.sample_pairs,
        failures: listed()
            .enumerate()
            .filter_map(|(line, account)| account.failure.as_ref().map(|reason| format!("accounts entry {}: {reason}", line + 1)))
            .collect(),
    })
}

enum Next {
    Done,
    /// A device whose registration has not completed: retried with its saved id.
    Finish(usize),
    Add,
}

fn next_registration(account: &AccountState, wanted: usize) -> Next {
    if let Some(device) = account.devices.iter().position(|device| !device.retired && device.device_id.is_none()) {
        return Next::Finish(device);
    }
    if account.devices.iter().filter(|device| !device.retired).count() < wanted {
        Next::Add
    } else {
        Next::Done
    }
}

fn store(state: &mut State, account: usize, device: usize, tokens: DeviceTokens) {
    let account = &mut state.accounts[account];
    account.user_id = Some(tokens.user_id);
    let device = &mut account.devices[device];
    device.device_id = Some(tokens.device_id);
    device.refresh_token = Some(tokens.refresh_token);
    device.needs_sign_in = false;
}

fn retire(state: &mut State, account: usize, device: usize) {
    let retired = &mut state.accounts[account].devices[device];
    retired.retired = true;
    retired.refresh_token = None;
    let at = DeviceRef { account, device };
    state.pairs.retain(|pair| pair.sender != at && pair.receiver != at);
}

async fn prepare_account(
    api: &AuthApi,
    shared: &Shared,
    index: usize,
    account: &Account,
    wanted: usize,
) -> Result<(), Failure> {
    // Devices a run found signed out: sign the same device in again.
    let repairs: Vec<(usize, Uuid)> = shared
        .read(|state| {
            state.accounts[index]
                .devices
                .iter()
                .enumerate()
                .filter(|(_, device)| device.needs_sign_in && !device.retired)
                .filter_map(|(position, device)| device.device_id.map(|id| (position, id)))
                .collect()
        })
        .await;
    for (device, device_id) in repairs {
        match api.login_device(&account.email, &account.password, device_id).await {
            Ok(tokens) => shared.update(|state| store(state, index, device, tokens)).await?,
            Err(ApiError::Rejected { status: 404, .. }) => shared.update(|state| retire(state, index, device)).await?,
            Err(error) => return Err(classify(error)),
        }
    }
    let mut registration: Option<String> = None;
    let mut renewed = false;
    loop {
        let next = shared.read(|state| next_registration(&state.accounts[index], wanted)).await;
        let device = match next {
            Next::Done => return Ok(()),
            Next::Finish(device) => device,
            // Saved before the request: a retry after a crash reuses its registration id.
            Next::Add => {
                shared
                    .update(|state| {
                        let devices = &mut state.accounts[index].devices;
                        devices.push(DeviceState::new());
                        devices.len() - 1
                    })
                    .await?
            }
        };
        let registration_id = shared.read(|state| state.accounts[index].devices[device].registration_id).await;
        let token = match &registration {
            Some(token) => token.clone(),
            None => {
                let token = api.login(&account.email, &account.password).await.map_err(classify)?;
                registration = Some(token.clone());
                token
            }
        };
        let name = format!("push-load {}", device + 1);
        match api.register_device(&token, registration_id, &name).await {
            Ok(tokens) => shared.update(|state| store(state, index, device, tokens)).await?,
            // The registration token lasts 5 minutes: sign in again once.
            Err(ApiError::Rejected { status: 401, .. }) if !renewed => {
                registration = None;
                renewed = true;
            }
            // The id of a revoked device stays spent.
            Err(ApiError::Rejected { status: 404, .. }) => shared.update(|state| retire(state, index, device)).await?,
            Err(error) => {
                if matches!(error, ApiError::Rejected { status: 409, .. }) {
                    shared.update(|state| retire(state, index, device)).await?;
                }
                return Err(classify(error));
            }
        }
    }
}

async fn prepare_pairs(
    api: &AuthApi,
    target: &Target,
    shared: &Shared,
    indices: &[usize],
    wanted: usize,
) -> anyhow::Result<usize> {
    shared
        .update(|state| {
            let kept: Vec<Pair> = state.pairs.iter().filter(|pair| state.pair_is_ready(pair)).cloned().collect();
            state.pairs = kept;
        })
        .await?;
    let mut ready = shared.read(|state| state.pairs.len()).await;
    let paired: HashSet<usize> = shared
        .read(|state| state.pairs.iter().flat_map(|pair| [pair.sender.account, pair.receiver.account]).collect())
        .await;
    let free: Vec<DeviceRef> = shared
        .read(|state| {
            indices
                .iter()
                .filter(|index| !paired.contains(index) && state.accounts[**index].failure.is_none())
                .filter_map(|&account| {
                    state.accounts[account]
                        .devices
                        .iter()
                        .position(DeviceState::is_ready)
                        .map(|device| DeviceRef { account, device })
                })
                .collect()
        })
        .await;
    for chunk in free.chunks(2) {
        if ready >= wanted {
            break;
        }
        let [sender, receiver] = chunk else { break };
        let conversation_id = make_pair(api, target, shared, *sender, *receiver).await?;
        shared
            .update(|state| state.pairs.push(Pair { sender: *sender, receiver: *receiver, conversation_id }))
            .await?;
        ready += 1;
    }
    Ok(ready)
}

/// Refreshes `device`, saving the rotated token before using it, and connects it,
/// riding out 4006 while the worker relays a new device.
async fn connect_device(api: &AuthApi, target: &Target, shared: &Shared, device: DeviceRef) -> anyhow::Result<(Socket, Ready)> {
    let refresh_token = shared
        .read(|state| state.device(device).refresh_token.clone())
        .await
        .context("a paired device has no refresh token")?;
    let tokens = match api.refresh(&refresh_token).await {
        Ok(tokens) => tokens,
        Err(ApiError::Rejected { status: 401, .. }) => {
            shared.update(|state| state.device_mut(device).needs_sign_in = true).await?;
            bail!("a device's refresh token was refused; run prepare again to sign it in");
        }
        Err(error) => return Err(error.into()),
    };
    let access_token = tokens.access_token.clone();
    shared
        .update(|state| state.device_mut(device).refresh_token = Some(tokens.refresh_token))
        .await?;
    let deadline = Instant::now() + READY_WAIT;
    loop {
        match socket::open(target, &access_token).await? {
            Opened::Ready(socket, ready) => return Ok((*socket, ready)),
            Opened::Closed(4006) if Instant::now() < deadline => tokio::time::sleep(Duration::from_secs(1)).await,
            Opened::Closed(code) => bail!("the gateway refused a prepared device with close code {code}"),
        }
    }
}

/// The direct conversation of two accounts, made contacts by an invite when they are not yet.
async fn make_pair(api: &AuthApi, target: &Target, shared: &Shared, sender: DeviceRef, receiver: DeviceRef) -> anyhow::Result<Uuid> {
    let (mut a, _) = connect_device(api, target, shared, sender).await?;
    let result = async {
        let (mut b, b_ready) = connect_device(api, target, shared, receiver).await?;
        let conversation = async {
            let open = request::Body::OpenDirect(OpenDirect { peer_user_id: id_bytes(b_ready.user) });
            if let Ok(response::Body::DirectOpened(opened)) = socket::ask(&mut a, 1, open.clone()).await? {
                return id(&opened.conversation_id).map_err(|_| anyhow::anyhow!("an invalid conversation id"));
            }
            let create = request::Body::CreateInvite(CreateInvite { expires_in_s: 600, max_uses: 1 });
            let invite = match socket::ask(&mut a, 2, create).await? {
                Ok(response::Body::Invite(invite)) => invite,
                Ok(_) => bail!("creating an invite was answered with something else"),
                Err(error) => bail!("creating an invite was refused: {:?}", error.code()),
            };
            match socket::ask(&mut b, 1, request::Body::RedeemInvite(RedeemInvite { code: invite.code })).await? {
                Ok(response::Body::InviteRedeemed(_)) => {}
                Ok(_) => bail!("redeeming the invite was answered with something else"),
                Err(error) => bail!(
                    "redeeming the invite was refused: {:?} (retry after {:?} s)",
                    error.code(),
                    error.retry_after_s
                ),
            }
            match socket::ask(&mut a, 3, open).await? {
                Ok(response::Body::DirectOpened(opened)) => {
                    id(&opened.conversation_id).map_err(|_| anyhow::anyhow!("an invalid conversation id"))
                }
                _ => bail!("the direct conversation could not be opened"),
            }
        }
        .await;
        socket::close(b).await;
        conversation
    }
    .await;
    socket::close(a).await;
    result
}
```

- [ ] **Step 2: Build, lint and commit.**

```powershell
cargo build -p push-load --locked
cargo clippy -p push-load --all-targets --locked -- -D warnings
git add crates\push-load
git commit -m "feat: prepare push-load devices and sample pairs resumably" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 6: The measured run

- [ ] **Step 1: Write the failing close-classification test** at the end of
  `crates\push-load\src\run.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes_decide_whether_a_connection_reconnects() {
        for code in [4001, 4002, 4003] {
            assert_eq!(close_action(code), CloseAction::AuthFailure, "{code}");
        }
        assert_eq!(close_action(4004), CloseAction::RateLimited);
        assert_eq!(close_action(4007), CloseAction::Takeover);
        for code in [1000, 1001, 1002, 1006, 1008, 1011, 4005, 4006] {
            assert_eq!(close_action(code), CloseAction::Reconnect, "{code}");
        }
    }

    #[test]
    fn jitter_stays_between_half_and_one_and_a_half() {
        for _ in 0..100 {
            let pause = jitter(Duration::from_secs(2));
            assert!((Duration::from_secs(1)..=Duration::from_secs(3)).contains(&pause), "{pause:?}");
        }
    }
}
```

- [ ] **Step 2: Run red.**

```powershell
cargo test -p push-load --lib run --locked
```

Expected: compile errors (`close_action`, `jitter` undefined).

- [ ] **Step 3: Implement `run.rs`** above the tests:

```rust
//! A measured run. It refreshes the selected devices' tokens once (saving the rotated
//! refresh tokens before connecting), opens their connections at a fixed rate, keeps
//! them alive with application Pings every announced heartbeat for the duration, sends
//! sample messages between the prepared pairs, reconnects connections the target drops
//! (a gateway rollout), then closes every socket and reports. It never re-authenticates,
//! discovers targets or creates accounts.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use push_proto::v1::frame::Body;
use push_proto::v1::{Ack, ContentType, ErrorCode, Ping, Send, receipt};
use push_proto::{decode_frame, id_bytes};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::{RunOptions, Target};
use crate::http::{ApiError, AuthApi};
use crate::metrics::{self, CloseCodes, Counters, LAG_LIMIT, Messages, PERCENTILES, Samples, bump, read};
use crate::report::{self, Connections, Generator, Latency, Report, Status, Workload};
use crate::socket::{self, ABNORMAL_CLOSE, Opened, Ready, Socket};
use crate::state::{DeviceRef, State};

const REFRESH_CONCURRENCY: usize = 32;
/// Tokens are refreshed right before connecting; a slower refresh would eat into their life.
const REFRESH_PHASE_LIMIT: Duration = Duration::from_secs(180);
const SAVE_EVERY: usize = 256;
/// A connection must reach Ready within this long of its first attempt or of losing it.
const READY_DEADLINE: Duration = Duration::from_secs(60);
const PONG_TIMEOUT: Duration = Duration::from_secs(10);
/// No sample message is sent in the last seconds of the run, so all can arrive.
const DRAIN: Duration = Duration::from_secs(10);
const STOP_GRACE: Duration = Duration::from_secs(15);
/// Acks are coalesced (the highest seq per conversation) and sent this often, inside the
/// gateway's frame budget even for a large backlog.
const ACK_EVERY: Duration = Duration::from_millis(250);
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    Reconnect,
    AuthFailure,
    Takeover,
    RateLimited,
}

/// What a close code means for a connection (`push_proto::close`).
pub fn close_action(code: u16) -> CloseAction {
    match code {
        4001..=4003 => CloseAction::AuthFailure,
        4004 => CloseAction::RateLimited,
        4007 => CloseAction::Takeover,
        _ => CloseAction::Reconnect,
    }
}

/// `base` scaled by a random factor from 0.5 to 1.5.
fn jitter(base: Duration) -> Duration {
    let random = Uuid::new_v4().as_bytes()[0];
    base.mul_f64(0.5 + f64::from(random) / 255.0)
}

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Cancelled,
    Closed(u16),
    PongTimeout,
    Undecodable,
}

struct Shared {
    target: Target,
    options: RunOptions,
    cancel: CancellationToken,
    stop_sending: AtomicBool,
    heartbeat_ms: AtomicU64,
    counters: Counters,
    closes: CloseCodes,
    refusals: CloseCodes,
    connect: Samples,
    ping_rtt: Samples,
    reconnect: Samples,
    messages: Messages,
}

struct Plan {
    access_token: String,
    /// The conversation a pair's sender sends sample messages to.
    sends_to: Option<Vec<u8>>,
}

pub async fn run(target: &Target, state_path: &Path, options: &RunOptions) -> anyhow::Result<Report> {
    options.validate()?;
    crate::state::ensure_outside_git(state_path)?;
    let mut state = State::load(state_path)?.context("there is no state file: run prepare first")?;
    if state.target != target.authority() {
        bail!("{} was prepared for {}, not {}", state_path.display(), state.target, target.authority());
    }
    let ready = state.ready_devices();
    if ready.len() < options.connections {
        bail!(
            "only {} prepared devices are ready; --connections {} needs more (run prepare with more accounts or devices)",
            ready.len(),
            options.connections
        );
    }
    let selected: Vec<DeviceRef> = ready[..options.connections].to_vec();
    let chosen: HashSet<DeviceRef> = selected.iter().copied().collect();
    let sends_to: HashMap<DeviceRef, Vec<u8>> = state
        .pairs
        .iter()
        .filter(|pair| state.pair_is_ready(pair) && chosen.contains(&pair.sender) && chosen.contains(&pair.receiver))
        .map(|pair| (pair.sender, id_bytes(pair.conversation_id)))
        .collect();
    let started_unix_s = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let started = Instant::now();
    let shared = Arc::new(Shared {
        target: target.clone(),
        options: options.clone(),
        cancel: CancellationToken::new(),
        stop_sending: AtomicBool::new(false),
        heartbeat_ms: AtomicU64::new(0),
        counters: Counters::default(),
        closes: CloseCodes::default(),
        refusals: CloseCodes::default(),
        connect: Samples::default(),
        ping_rtt: Samples::default(),
        reconnect: Samples::default(),
        messages: Messages::default(),
    });

    let tokens = refresh_all(target, &mut state, state_path, &selected, &shared).await?;

    let interrupt = CancellationToken::new();
    let on_interrupt = interrupt.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_interrupt.cancel();
        }
    });
    let lag = Arc::new(Samples::default());
    let lag_cancel = CancellationToken::new();
    let lag_watch = tokio::spawn(metrics::watch_lag(lag.clone(), lag_cancel.clone()));

    let mut tasks = JoinSet::new();
    let ramp_start = Instant::now();
    let interval = Duration::from_secs_f64(1.0 / f64::from(options.ramp_per_second));
    for (index, (device, token)) in selected.iter().zip(tokens).enumerate() {
        let due = ramp_start + interval.mul_f64(index as f64);
        tokio::select! {
            () = interrupt.cancelled() => break,
            () = tokio::time::sleep_until(due) => {}
        }
        // A device whose refresh failed is counted already and not connected.
        let Some(access_token) = token else { continue };
        let plan = Plan { access_token, sends_to: sends_to.get(device).cloned() };
        tasks.spawn(connection(shared.clone(), plan));
    }
    let ramp_end = Instant::now();

    let mut min_active: Option<i64> = None;
    if !interrupt.is_cancelled() {
        let run_end = ramp_end + options.duration;
        let stop_sending_at = (run_end - DRAIN).max(ramp_end);
        let mut sampler = tokio::time::interval(Duration::from_secs(1));
        sampler.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = interrupt.cancelled() => break,
                () = tokio::time::sleep_until(run_end) => break,
                _ = sampler.tick() => {
                    if Instant::now() >= stop_sending_at {
                        shared.stop_sending.store(true, Ordering::SeqCst);
                    }
                    let active = shared.counters.active();
                    min_active = Some(min_active.map_or(active, |least| least.min(active)));
                }
            }
        }
    }
    shared.stop_sending.store(true, Ordering::SeqCst);
    shared.cancel.cancel();
    let stop_deadline = Instant::now() + STOP_GRACE;
    loop {
        match tokio::time::timeout_at(stop_deadline, tasks.join_next()).await {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                tasks.abort_all();
                break;
            }
        }
    }
    lag_cancel.cancel();
    let _ = lag_watch.await;

    let c = &shared.counters;
    let lag = lag.summary();
    let heartbeat_ms = shared.heartbeat_ms.load(Ordering::SeqCst);
    let mut report = Report {
        tool: "push-load".to_owned(),
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        status: Status::Failed,
        failures: Vec::new(),
        target: target.base_url(),
        target_build: options.target_build.trim().to_owned(),
        started_unix_s,
        elapsed_s: started.elapsed().as_secs_f64(),
        workload: Workload {
            connections: options.connections,
            ramp_per_second: options.ramp_per_second,
            ramp_s: (ramp_end - ramp_start).as_secs_f64(),
            duration_s: options.duration.as_secs(),
            heartbeat_ms: (heartbeat_ms > 0).then_some(heartbeat_ms),
            sample_pairs: sends_to.len(),
            message_interval_s: options.message_interval.as_secs(),
            reconnect: options.reconnect,
        },
        connections: Connections {
            ready_once: read(&c.ready_once),
            attempt_errors: read(&c.attempt_errors),
            connect_failures: read(&c.connect_failures),
            reconnect_failures: read(&c.reconnect_failures),
            auth_failures: read(&c.auth_failures),
            takeovers: read(&c.takeovers),
            rate_limited: read(&c.rate_limited),
            pong_timeouts: read(&c.pong_timeouts),
            protocol_failures: read(&c.protocol_failures),
            disconnects: read(&c.disconnects),
            reconnects: read(&c.reconnects),
            pings: read(&c.pings),
            pongs: read(&c.pongs),
            auth_expiring: read(&c.auth_expiring),
            error_frames: read(&c.error_frames),
            peak_active: c.peak(),
            min_active_after_ramp: min_active,
        },
        close_codes: shared.closes.snapshot(),
        ready_refusals: shared.refusals.snapshot(),
        messages: shared.messages.counts(),
        latency: Latency {
            connect: shared.connect.summary(),
            ping_rtt: shared.ping_rtt.summary(),
            delivery: shared.messages.delivery.summary(),
            receipt: shared.messages.receipt.summary(),
            reconnect: shared.reconnect.summary(),
        },
        generator: Generator {
            saturated: lag.p99_ms.is_some_and(|p99| u128::from(p99) > LAG_LIMIT.as_millis()),
            scheduling_lag: lag,
            lag_limit_ms: u64::try_from(LAG_LIMIT.as_millis()).unwrap_or(u64::MAX),
        },
        percentiles: PERCENTILES.to_owned(),
    };
    let (status, failures) = report::judge(&report, interrupt.is_cancelled());
    report.status = status;
    report.failures = failures;
    Ok(report)
}

/// One refresh per selected device: the access tokens, in `selected` order (`None` when
/// refused). Rotated refresh tokens are saved before any connection uses them; a refused
/// one marks its device for `prepare` to sign in again.
async fn refresh_all(
    target: &Target,
    state: &mut State,
    path: &Path,
    selected: &[DeviceRef],
    shared: &Shared,
) -> anyhow::Result<Vec<Option<String>>> {
    let api = AuthApi::new(target)?;
    let deadline = Instant::now() + REFRESH_PHASE_LIMIT;
    let limit = Arc::new(Semaphore::new(REFRESH_CONCURRENCY));
    let mut jobs = JoinSet::new();
    for (position, device) in selected.iter().enumerate() {
        let token = state
            .device(*device)
            .refresh_token
            .clone()
            .context("a ready device has no refresh token")?;
        let (api, limit) = (api.clone(), limit.clone());
        jobs.spawn(async move {
            let _permit = limit.acquire_owned().await.expect("the semaphore stays open");
            (position, api.refresh(&token).await)
        });
    }
    let mut access = vec![None; selected.len()];
    let mut unsaved = 0;
    loop {
        let joined = match tokio::time::timeout_at(deadline, jobs.join_next()).await {
            Ok(Some(joined)) => joined.context("a refresh task panicked")?,
            Ok(None) => break,
            Err(_) => {
                jobs.abort_all();
                while jobs.join_next().await.is_some() {}
                state.save(path)?;
                bail!(
                    "refreshing the devices' tokens took longer than {} s; nothing was connected. Devices whose answers were cut off need `prepare` again after 30 s",
                    REFRESH_PHASE_LIMIT.as_secs()
                );
            }
        };
        let (position, outcome) = joined;
        let device = selected[position];
        match outcome {
            Ok(tokens) => {
                state.device_mut(device).refresh_token = Some(tokens.refresh_token);
                access[position] = Some(tokens.access_token);
            }
            Err(ApiError::Rejected { status: 401, .. }) => {
                state.device_mut(device).needs_sign_in = true;
                bump(&shared.counters.auth_failures);
            }
            Err(_) => bump(&shared.counters.connect_failures),
        }
        unsaved += 1;
        if unsaved >= SAVE_EVERY {
            state.save(path)?;
            unsaved = 0;
        }
    }
    state.save(path)?;
    Ok(access)
}

/// One device's connection for the whole run: connect, keep it alive, reconnect when the
/// target drops it, stop on cancellation or on a close that must not be retried.
async fn connection(shared: Arc<Shared>, plan: Plan) {
    let first_attempt = Instant::now();
    let mut ever_ready = false;
    let mut lost_at: Option<Instant> = None;
    let mut deadline = first_attempt + READY_DEADLINE;
    let mut backoff = FIRST_BACKOFF;
    loop {
        let opened = tokio::select! {
            () = shared.cancel.cancelled() => return,
            opened = socket::open(&shared.target, &plan.access_token) => opened,
        };
        match opened {
            Ok(Opened::Ready(socket, ready)) => {
                if ever_ready {
                    if let Some(at) = lost_at.take() {
                        shared.reconnect.record(at.elapsed());
                        bump(&shared.counters.reconnects);
                    }
                } else {
                    ever_ready = true;
                    shared.connect.record(first_attempt.elapsed());
                    bump(&shared.counters.ready_once);
                }
                let heartbeat = u64::try_from(ready.heartbeat.as_millis()).unwrap_or(u64::MAX);
                shared.heartbeat_ms.store(heartbeat, Ordering::SeqCst);
                shared.counters.opened();
                let ended = session(&shared, &plan, *socket, ready).await;
                shared.counters.closed();
                match ended {
                    Ended::Cancelled => return,
                    Ended::Closed(code) => {
                        shared.closes.record(code);
                        bump(&shared.counters.disconnects);
                        if stop_for(&shared, code) {
                            return;
                        }
                    }
                    Ended::PongTimeout => {
                        bump(&shared.counters.pong_timeouts);
                        bump(&shared.counters.disconnects);
                    }
                    Ended::Undecodable => {
                        bump(&shared.counters.protocol_failures);
                        bump(&shared.counters.disconnects);
                    }
                }
                if !shared.options.reconnect {
                    return;
                }
                lost_at = Some(Instant::now());
                deadline = Instant::now() + READY_DEADLINE;
                backoff = FIRST_BACKOFF;
            }
            Ok(Opened::Closed(code)) => {
                shared.refusals.record(code);
                if stop_for(&shared, code) {
                    return;
                }
            }
            Err(_) => bump(&shared.counters.attempt_errors),
        }
        if Instant::now() >= deadline {
            bump(if ever_ready { &shared.counters.reconnect_failures } else { &shared.counters.connect_failures });
            return;
        }
        tokio::select! {
            () = shared.cancel.cancelled() => return,
            () = tokio::time::sleep(jitter(backoff)) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Counts a close that must not be retried; whether the connection stops.
fn stop_for(shared: &Shared, code: u16) -> bool {
    let counter = match close_action(code) {
        CloseAction::Reconnect => return false,
        CloseAction::AuthFailure => &shared.counters.auth_failures,
        CloseAction::Takeover => &shared.counters.takeovers,
        CloseAction::RateLimited => &shared.counters.rate_limited,
    };
    bump(counter);
    true
}

/// An identified connection until it ends: application Pings every heartbeat, sample
/// messages for a pair's sender, coalesced Acks for everything delivered.
async fn session(shared: &Shared, plan: &Plan, mut socket: Socket, ready: Ready) -> Ended {
    let own = id_bytes(ready.user);
    let now = Instant::now();
    let mut pings = tokio::time::interval_at(now + ready.heartbeat, ready.heartbeat);
    pings.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let every = shared.options.message_interval;
    let mut sends = tokio::time::interval_at(now + every, every);
    sends.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut acks = tokio::time::interval(ACK_EVERY);
    acks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut to_ack: HashMap<Vec<u8>, i64> = HashMap::new();
    let mut ping_sent: Option<Instant> = None;
    loop {
        let pong_due = ping_sent.map_or_else(|| Instant::now() + Duration::from_secs(86_400), |sent| sent + PONG_TIMEOUT);
        let outgoing = tokio::select! {
            () = shared.cancel.cancelled() => {
                socket::close(socket).await;
                return Ended::Cancelled;
            }
            () = tokio::time::sleep_until(pong_due) => return Ended::PongTimeout,
            _ = pings.tick() => {
                if ping_sent.is_some() {
                    continue;
                }
                ping_sent = Some(Instant::now());
                bump(&shared.counters.pings);
                vec![Body::Ping(Ping {})]
            }
            _ = sends.tick(), if plan.sends_to.is_some() => {
                if shared.stop_sending.load(Ordering::SeqCst) {
                    continue;
                }
                let conversation = plan.sends_to.clone().expect("guarded by the branch condition");
                let client_msg_id = id_bytes(Uuid::now_v7());
                shared.messages.sent(client_msg_id.clone(), conversation.clone());
                vec![Body::Send(Send {
                    client_msg_id,
                    conversation_id: conversation,
                    content_type: ContentType::Text.into(),
                    body: format!("push-load {}", Uuid::now_v7()).into_bytes(),
                    client_ts: None,
                })]
            }
            _ = acks.tick() => to_ack
                .drain()
                .map(|(conversation_id, seq)| Body::Ack(Ack { conversation_id, seq }))
                .collect(),
            message = socket.next() => {
                match message {
                    Some(Ok(Message::Binary(bytes))) => {
                        let Ok(body) = decode_frame(&bytes) else { return Ended::Undecodable };
                        match body {
                            Body::Pong(_) => {
                                if let Some(sent) = ping_sent.take() {
                                    shared.ping_rtt.record(sent.elapsed());
                                    bump(&shared.counters.pongs);
                                }
                            }
                            Body::SendAck(ack) => shared.messages.stored(&ack.client_msg_id, ack.seq),
                            Body::Deliver(entry) => {
                                if entry.sender_id != own {
                                    shared.messages.delivered(&entry.client_msg_id);
                                }
                                let highest = to_ack.entry(entry.conversation_id).or_insert(entry.seq);
                                *highest = (*highest).max(entry.seq);
                            }
                            Body::Receipt(receipt) => {
                                if let Some(receipt::Positions::Direct(direct)) = receipt.positions {
                                    shared.messages.receipted(&receipt.conversation_id, direct.delivered_seq);
                                }
                            }
                            Body::AuthExpiring(_) => bump(&shared.counters.auth_expiring),
                            Body::Error(error) => {
                                bump(&shared.counters.error_frames);
                                if error.code() == ErrorCode::RateLimited {
                                    bump(&shared.counters.rate_limited);
                                }
                            }
                            _ => {}
                        }
                        Vec::new()
                    }
                    Some(Ok(Message::Close(frame))) => return Ended::Closed(socket::close_code(frame)),
                    Some(Ok(_)) => Vec::new(),
                    Some(Err(_)) | None => return Ended::Closed(ABNORMAL_CLOSE),
                }
            }
        };
        for body in outgoing {
            if socket::send(&mut socket, body).await.is_err() {
                return Ended::Closed(ABNORMAL_CLOSE);
            }
        }
    }
}
```

- [ ] **Step 4: Run green, lint and commit.**

```powershell
cargo test -p push-load --lib --locked
cargo clippy -p push-load --all-targets --locked -- -D warnings
git add crates\push-load
git commit -m "feat: run bounded push-load measurements" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 7: Command line

- [ ] **Step 1: Write the failing parser tests** at the end of `crates\push-load\src\cli.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn flags_are_explicit_and_never_repeated() {
        let flags = parse(&args(&["--state", "s.json", "--no-reconnect"]), &["--state"], &["--no-reconnect"]).unwrap();
        assert_eq!(flags.required("--state").unwrap(), "s.json");
        assert!(flags.switch("--no-reconnect"));
        assert!(parse(&args(&["--unknown", "x"]), &["--state"], &[]).is_err());
        assert!(parse(&args(&["--state"]), &["--state"], &[]).is_err());
        assert!(parse(&args(&["--state", "a", "--state", "b"]), &["--state"], &[]).is_err());
        assert!(flags.required("--target").is_err());
        assert_eq!(flags.number::<u32>("--count", Some(3)).unwrap(), 3);
        assert!(flags.number::<u32>("--count", None).is_err());
    }

    #[tokio::test]
    async fn a_mismatched_authorization_stops_before_any_request() {
        let error = main(args(&[
            "run", "--target", "https://api.example.com", "--authorized-target", "other.example.com",
            "--state", "unused.json", "--connections", "1", "--ramp-per-second", "1",
            "--duration-seconds", "10", "--target-build", "x", "--report", "unused-report.json",
        ]))
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("--authorized-target"), "{error}");
    }
}
```

- [ ] **Step 2: Run red** (`cargo test -p push-load --lib cli --locked`): compile errors.

- [ ] **Step 3: Implement `cli.rs`** above the tests and `src\main.rs`:

```rust
//! The command line. Credentials come only from files, never from arguments.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::config::{PrepareOptions, RunOptions, Target};
use crate::report::Status;
use crate::{accounts, prepare, report, run};

pub const USAGE: &str = "\
usage:
  push-load prepare --target <url> --authorized-target <host[:port]> --accounts <file.jsonl>
                    --state <file.json> --devices-per-account <1-10>
                    [--sample-pairs <0-50>] [--concurrency <1-16>] [--connect-to <ip>]
  push-load run     --target <url> --authorized-target <host[:port]> --state <file.json>
                    --connections <n> --ramp-per-second <n> --duration-seconds <n>
                    --target-build <label> --report <new-file.json>
                    [--message-interval-seconds <n>] [--connect-to <ip>] [--no-reconnect]

Only test a deployment you own or are authorized to test (docs/consumer-verification.md).";

struct Flags {
    values: HashMap<String, String>,
    switches: Vec<String>,
}

fn parse(args: &[String], values: &[&str], switches: &[&str]) -> anyhow::Result<Flags> {
    let mut flags = Flags { values: HashMap::new(), switches: Vec::new() };
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if switches.contains(&flag.as_str()) {
            flags.switches.push(flag.clone());
            continue;
        }
        if !values.contains(&flag.as_str()) {
            bail!("unknown argument {flag:?}\n{USAGE}");
        }
        let value = args.next().with_context(|| format!("{flag} needs a value"))?;
        if flags.values.insert(flag.clone(), value.clone()).is_some() {
            bail!("{flag} is given twice");
        }
    }
    Ok(flags)
}

impl Flags {
    fn required(&self, flag: &str) -> anyhow::Result<&str> {
        self.values
            .get(flag)
            .map(String::as_str)
            .with_context(|| format!("{flag} is required\n{USAGE}"))
    }

    fn optional(&self, flag: &str) -> Option<&str> {
        self.values.get(flag).map(String::as_str)
    }

    fn switch(&self, flag: &str) -> bool {
        self.switches.iter().any(|given| given == flag)
    }

    fn number<T: FromStr>(&self, flag: &str, default: Option<T>) -> anyhow::Result<T> {
        match (self.optional(flag), default) {
            (Some(value), _) => value.parse().map_err(|_| anyhow::anyhow!("{flag} must be a number")),
            (None, Some(default)) => Ok(default),
            (None, None) => bail!("{flag} is required\n{USAGE}"),
        }
    }

    fn target(&self) -> anyhow::Result<Target> {
        Target::new(self.required("--target")?, self.required("--authorized-target")?, self.optional("--connect-to"))
    }
}

/// Runs `push-load <args>`: exit 0 when the preparation is complete or the run passed,
/// 1 when not, 130 when interrupted, 2 for usage.
pub async fn main(args: Vec<String>) -> anyhow::Result<ExitCode> {
    match args.first().map(String::as_str) {
        Some("prepare") => prepare_command(&args[1..]).await,
        Some("run") => run_command(&args[1..]).await,
        _ => {
            eprintln!("{USAGE}");
            Ok(ExitCode::from(2))
        }
    }
}

async fn prepare_command(args: &[String]) -> anyhow::Result<ExitCode> {
    let flags = parse(
        args,
        &["--target", "--authorized-target", "--accounts", "--state", "--devices-per-account", "--sample-pairs", "--concurrency", "--connect-to"],
        &[],
    )?;
    let target = flags.target()?;
    let options = PrepareOptions {
        devices_per_account: flags.number("--devices-per-account", None)?,
        sample_pairs: flags.number("--sample-pairs", Some(0))?,
        concurrency: flags.number("--concurrency", Some(4))?,
    };
    options.validate()?;
    let accounts = accounts::read(&PathBuf::from(flags.required("--accounts")?))?;
    let summary = prepare::prepare(&target, &accounts, &PathBuf::from(flags.required("--state")?), &options).await?;
    println!(
        "push-load: prepared {} of {} devices and {} of {} sample pairs for {} accounts at {}",
        summary.devices_ready, summary.devices_wanted, summary.pairs_ready, summary.pairs_wanted, summary.accounts, target.base_url()
    );
    for failure in &summary.failures {
        println!("  failure: {failure}");
    }
    Ok(if summary.complete() { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

async fn run_command(args: &[String]) -> anyhow::Result<ExitCode> {
    let flags = parse(
        args,
        &[
            "--target", "--authorized-target", "--state", "--connections", "--ramp-per-second",
            "--duration-seconds", "--message-interval-seconds", "--target-build", "--report", "--connect-to",
        ],
        &["--no-reconnect"],
    )?;
    let target = flags.target()?;
    let options = RunOptions {
        connections: flags.number("--connections", None)?,
        ramp_per_second: flags.number("--ramp-per-second", None)?,
        duration: Duration::from_secs(flags.number("--duration-seconds", None)?),
        message_interval: Duration::from_secs(flags.number("--message-interval-seconds", Some(10))?),
        target_build: flags.required("--target-build")?.to_owned(),
        reconnect: !flags.switch("--no-reconnect"),
    };
    options.validate()?;
    let report_path = PathBuf::from(flags.required("--report")?);
    if report_path.exists() {
        bail!("{} exists: name a new report file", report_path.display());
    }
    let report = run::run(&target, &PathBuf::from(flags.required("--state")?), &options).await?;
    let json = serde_json::to_string_pretty(&report).context("serialize the report")?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&report_path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, json.as_bytes()))
        .with_context(|| format!("write the report {}", report_path.display()))?;
    print!("{}", report::summary_text(&report));
    println!("  report: {}", report_path.display());
    Ok(match report.status {
        Status::Passed => ExitCode::SUCCESS,
        Status::Failed => ExitCode::from(1),
        Status::Interrupted => ExitCode::from(130),
    })
}
```

`crates\push-load\src\main.rs`:

```rust
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match push_load::cli::main(std::env::args().skip(1).collect()).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("push-load: {error:#}");
            ExitCode::from(2)
        }
    }
}
```

- [ ] **Step 4: Run green, lint, check the usage path and commit.**

```powershell
cargo test -p push-load --locked
cargo clippy -p push-load --all-targets --locked -- -D warnings
cargo run -q -p push-load --locked; "exit=$LASTEXITCODE"
git add crates\push-load
git commit -m "feat: add the push-load command line" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

Expected: tests pass; the bare command prints the usage and `exit=2`.

## Task 8: DEV ONLY accounts and the whole-stack correctness test

Verified accounts need the emailed link. Production tools must not read a mailbox, so
account creation stays in the DEV ONLY `push-smoke` crate, beside the smoke test that
already reads the worker's DEV ONLY mailbox.

- [ ] **Step 1: Add the DEV ONLY helper.** In `crates\push-smoke\Cargo.toml` add
  `push-load.workspace = true` to `[dependencies]`. In `crates\push-smoke\src\auth.rs`
  add to `impl<'a> Auth<'a>`:

```rust
    /// DEV ONLY: signs a new account up and verifies it with the link the worker's log
    /// sender delivered to the dev mailbox; its email and password.
    pub async fn sign_up_verified(&self, name: &str) -> anyhow::Result<(String, String)> {
        let suffix = Uuid::now_v7().simple().to_string();
        let suffix = &suffix[suffix.len() - 10..];
        let email = format!("{name}.{suffix}@load.test");
        let password = format!("load-{}", Uuid::now_v7());
        expect(
            self.post(
                "/v1/auth/signup",
                json!({
                    "email": email,
                    "password": password,
                    "handle": format!("{name}_{suffix}"),
                    "display_name": name,
                }),
            ),
            StatusCode::ACCEPTED,
            "sign up",
        )
        .await?;
        self.verify_email(&email).await?;
        Ok((email, password))
    }
```

In `crates\push-smoke\src\lib.rs`, after `run`:

```rust
/// DEV ONLY: `count` new verified accounts for push-load, through auth-svc's public API
/// and the worker's dev mailbox. auth-svc allows 20 signups per IP an hour.
pub async fn create_verified_accounts(
    target: &Target,
    count: usize,
) -> anyhow::Result<Vec<push_load::accounts::Account>> {
    let auth = auth::Auth::connect(target, STEP_TIMEOUT).await?;
    let mut accounts = Vec::with_capacity(count);
    for index in 0..count {
        let (email, password) = auth.sign_up_verified(&format!("load{index}")).await?;
        accounts.push(push_load::accounts::Account { email, password });
    }
    Ok(accounts)
}
```

Create `crates\push-smoke\src\bin\push-dev-accounts.rs`:

```rust
//! DEV ONLY: creates verified accounts for push-load on a local deployment whose worker
//! runs the log sender with the DEV ONLY mailbox (minikube). Never for a real deployment:
//! there, accounts come from the consumer's normal sign-up and verification.
//!
//!   push-dev-accounts --auth <url> --mailbox <url> --count <1-20> --out <file.jsonl> [--host <host>]

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, bail};

const USAGE: &str = "usage: push-dev-accounts --auth <url> --mailbox <url> --count <1-20> --out <file.jsonl> [--host <host>]";

#[tokio::main]
async fn main() -> ExitCode {
    match create(std::env::args().skip(1).collect()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("push-dev-accounts: FAIL: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn create(args: Vec<String>) -> anyhow::Result<()> {
    let (mut auth, mut mailbox, mut count, mut out, mut host) = (None, None, None, None, None);
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let slot = match flag.as_str() {
            "--auth" => &mut auth,
            "--mailbox" => &mut mailbox,
            "--count" => &mut count,
            "--out" => &mut out,
            "--host" => &mut host,
            other => bail!("unknown argument {other:?}; {USAGE}"),
        };
        *slot = Some(args.next().with_context(|| format!("{flag} needs a value"))?);
    }
    let count: usize = count.context(USAGE)?.parse().context("--count must be a number")?;
    if !(1..=20).contains(&count) {
        bail!("--count must be 1 to 20: auth-svc allows 20 signups per IP an hour");
    }
    let out = PathBuf::from(out.context(USAGE)?);
    push_load::state::ensure_outside_git(&out)?;
    if out.exists() {
        bail!("{} exists: name a new file", out.display());
    }
    let target = push_smoke::Target {
        auth_url: auth.context(USAGE)?,
        mailbox_url: mailbox.context(USAGE)?,
        socket_url: String::new(),
        host,
        signing_key: String::new(),
        data_dir: String::new(),
    };
    let accounts = push_smoke::create_verified_accounts(&target, count).await?;
    push_load::state::write_private(&out, push_load::accounts::to_lines(&accounts).as_bytes())?;
    println!("push-dev-accounts: wrote {} DEV ONLY verified accounts to {}", accounts.len(), out.display());
    Ok(())
}
```

- [ ] **Step 2: Refactor the whole-stack test into a reusable `Stack`.** Replace the
  body of `crates\push-smoke\tests\local.rs` from the imports through the end of
  `the_smoke_test_passes_against_the_whole_stack` with the following. The service wiring
  is unchanged; it gains a router so one origin serves both auth-svc and the gateway, as
  an ingress does. Keep `wait_until_ready` and `readyz_status` unchanged.

```rust
//! The whole system on this machine: Postgres and Redis in containers (Docker must be
//! running), auth-svc, the worker, message-svc and the gateway in this process, wired as
//! the chart wires them (each internal API behind its own token), and a router serving
//! both public APIs from one origin as an ingress does. The smoke test and push-load run
//! against them as clients see them.

use std::net::SocketAddr;
use std::time::Duration;

use push_load::config::{PrepareOptions, RunOptions, Target as LoadTarget};
use push_load::report::Status;
use push_load::state::State;
use push_smoke::{Target, run};
use push_store::StoreOptions;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

type Server = JoinHandle<Result<(), String>>;

struct Stack {
    auth_url: String,
    socket_url: String,
    worker_url: String,
    /// auth-svc and the gateway behind one origin, routed by path.
    public_url: String,
    signing_key: &'static str,
    shutdown: CancellationToken,
    servers: Vec<(&'static str, Server)>,
    _postgres: ContainerAsync<Postgres>,
    _redis: ContainerAsync<Redis>,
}

impl Stack {
    async fn start() -> Self {
        // (Move the existing body of the smoke test here, from starting Postgres through
        // the four wait_until_ready calls, with exactly two changes: each server is
        // spawned as `tokio::spawn(async move { <serve call>.await.map_err(|error| format!("{error:?}")) })`
        // so the handles share one type, and the router below is started last.)
        //
        // let router = TcpListener::bind("127.0.0.1:0").await.unwrap();
        // let public_url = format!("http://{}", router.local_addr().unwrap());
        // tokio::spawn(route(router, auth_http_addr, gateway_addr));
        //
        // Self { auth_url, socket_url, worker_url, public_url, signing_key, shutdown,
        //        servers: vec![("auth-svc", auth), ("the worker", worker),
        //                      ("message-svc", message_svc), ("the gateway", gateway)],
        //        _postgres: postgres, _redis: redis }
        unimplemented!("replaced by the moved setup")
    }

    fn smoke_target(&self, data_dir: String) -> Target {
        Target {
            mailbox_url: self.worker_url.clone(),
            auth_url: self.auth_url.clone(),
            socket_url: self.socket_url.clone(),
            host: None,
            signing_key: self.signing_key.to_owned(),
            data_dir,
        }
    }

    async fn stop(self) {
        self.shutdown.cancel();
        for (name, server) in self.servers {
            server
                .await
                .unwrap()
                .unwrap_or_else(|error| panic!("{name} did not stop cleanly: {error}"));
        }
    }
}

/// Routes each connection by its request line: the WebSocket to the gateway, everything
/// else to auth-svc (one HTTP client connection only ever carries auth-svc requests).
async fn route(listener: TcpListener, auth: SocketAddr, gateway: SocketAddr) {
    while let Ok((mut client, _)) = listener.accept().await {
        tokio::spawn(async move {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while !line.ends_with(b"\r\n") && line.len() < 8192 {
                if client.read_exact(&mut byte).await.is_err() {
                    return;
                }
                line.push(byte[0]);
            }
            let upstream = if line.starts_with(b"GET /v1/socket ") { gateway } else { auth };
            let Ok(mut upstream) = TcpStream::connect(upstream).await else { return };
            if upstream.write_all(&line).await.is_ok() {
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            }
        });
    }
}

#[tokio::test]
async fn the_smoke_test_passes_against_the_whole_stack() {
    let stack = Stack::start().await;
    // The SDK apps' databases, fresh for every run.
    let data_dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("push-smoke-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let target = stack.smoke_target(data_dir.to_string_lossy().into_owned());
    tokio::time::timeout(Duration::from_secs(300), run(&target))
        .await
        .expect("the smoke test finishes within 300 s")
        .expect("the smoke test passes");
    stack.stop().await;
    let _ = std::fs::remove_dir_all(&data_dir);
}
```

The `Stack::start` comment block is an instruction for this refactor, not code to keep:
the committed function contains the moved setup and returns the `Stack`. Record
`auth_http_addr = auth_http.local_addr().unwrap()` and
`gateway_addr = listener.local_addr().unwrap()` before the listeners move into their
servers.

- [ ] **Step 3: Write the harness correctness test** at the end of `local.rs`:

```rust
/// push-load against the whole stack: DEV ONLY accounts; a preparation with one unusable
/// account that resumes without new devices; a passing run with sample messages and no
/// credential in its report; a refused refresh token failing a run and repaired by the
/// next preparation; and too many connections refused before any request.
#[tokio::test]
async fn push_load_prepares_runs_and_reports_against_the_whole_stack() {
    let stack = Stack::start().await;
    let mut accounts = push_smoke::create_verified_accounts(&stack.smoke_target(String::new()), 4)
        .await
        .expect("four verified accounts");
    accounts.push(push_load::accounts::Account {
        email: format!("nobody.{}@load.test", uuid::Uuid::now_v7().simple()),
        password: "not-an-account".to_owned(),
    });
    // Credential files outside the repository: push-load refuses a Git working tree.
    let dir = std::env::temp_dir().join(format!("push-load-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let state_path = dir.join("state.json");
    let authority = stack.public_url.trim_start_matches("http://").to_owned();
    let target = LoadTarget::new(&stack.public_url, &authority, None).unwrap();
    let options = PrepareOptions { devices_per_account: 2, sample_pairs: 1, concurrency: 2 };

    let first = push_load::prepare::prepare(&target, &accounts, &state_path, &options).await.unwrap();
    assert_eq!((first.devices_ready, first.devices_wanted, first.pairs_ready), (8, 10, 1));
    assert_eq!(first.failures.len(), 1, "{:?}", first.failures);
    assert!(first.failures[0].contains("entry 5") && first.failures[0].contains("invalid_credentials"));
    assert!(!first.complete());
    let devices = |state: &State| -> Vec<Option<uuid::Uuid>> {
        state.accounts.iter().flat_map(|account| account.devices.iter().map(|device| device.device_id)).collect()
    };
    let prepared = State::load(&state_path).unwrap().unwrap();
    let again = push_load::prepare::prepare(&target, &accounts, &state_path, &options).await.unwrap();
    assert_eq!((again.devices_ready, again.pairs_ready), (8, 1));
    assert_eq!(devices(&State::load(&state_path).unwrap().unwrap()), devices(&prepared), "resuming adds no devices");

    let run_options = RunOptions {
        connections: 8,
        ramp_per_second: 8,
        duration: Duration::from_secs(35),
        message_interval: Duration::from_secs(2),
        target_build: "local whole-stack test".to_owned(),
        reconnect: true,
    };
    let report = push_load::run::run(&target, &state_path, &run_options).await.unwrap();
    assert_eq!(report.status, Status::Passed, "{:?}", report.failures);
    assert_eq!(report.connections.ready_once, 8);
    assert!(report.connections.pongs >= 8, "a Pong per connection after one 30 s heartbeat");
    assert!(report.messages.delivered >= 5 && report.messages.lost == 0, "{:?}", report.messages);
    assert!(report.latency.receipt.count >= 1);
    let json = serde_json::to_string(&report).unwrap();
    let state = State::load(&state_path).unwrap().unwrap();
    for account in &accounts {
        assert!(!json.contains(&account.email) && !json.contains(&account.password));
    }
    for device in state.accounts.iter().flat_map(|account| &account.devices) {
        if let Some(token) = &device.refresh_token {
            assert!(!json.contains(token.as_str()), "the report holds a refresh token");
        }
    }

    // A refused refresh token: counted as an authentication failure, never a pass.
    let mut broken = state.clone();
    let victim = broken.ready_devices()[4];
    broken.device_mut(victim).refresh_token = Some("not-a-refresh-token".to_owned());
    broken.save(&state_path).unwrap();
    let short = RunOptions { duration: Duration::from_secs(10), ..run_options.clone() };
    let failed = push_load::run::run(&target, &state_path, &short).await.unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert_eq!((failed.connections.auth_failures, failed.connections.ready_once), (1, 7));
    assert!(State::load(&state_path).unwrap().unwrap().device(victim).needs_sign_in);
    let repaired = push_load::prepare::prepare(&target, &accounts, &state_path, &options).await.unwrap();
    assert_eq!(repaired.devices_ready, 8);
    assert_eq!(devices(&State::load(&state_path).unwrap().unwrap()), devices(&prepared), "the same device signed in again");

    let too_many = RunOptions { connections: 9, ..run_options };
    let refused = push_load::run::run(&target, &state_path, &too_many).await.unwrap_err().to_string();
    assert!(refused.contains("prepared devices"), "{refused}");

    stack.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}
```

- [ ] **Step 4: Run both whole-stack tests, lint and commit.**

```powershell
cargo test -p push-smoke --test local --locked -- --nocapture
cargo clippy -p push-smoke -p push-load --all-targets --locked -- -D warnings
git add crates\push-smoke Cargo.lock
git commit -m "test: verify push-load against the whole local stack" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

Expected: both tests pass (the harness test takes about 1.5 minutes: two runs of 35 s
and 10 s plus preparation). A failure here is a harness or service defect to fix, not
a threshold to relax.

## Task 9: Image target and contributor CI

- [ ] **Step 1: Build `push-load` into the shared builder and add a `load` target** in
  `deploy\docker\Dockerfile`. Add `-p push-load` to the `cargo build` line and
  `target/release/push-load` to the `cp` line, add to the header comment

```dockerfile
#   docker build -f deploy/docker/Dockerfile --target load -t push-messenger/load:dev .
```

and append:

```dockerfile
# push-load, the explicitly targeted load harness (docs/consumer-verification.md). It
# needs a writable mounted directory for its state file and report.
FROM runtime AS load
COPY --from=builder /out/push-load /usr/local/bin/push-load
ENTRYPOINT ["/usr/local/bin/push-load"]
```

- [ ] **Step 2: Keep contributor CI building every target.** In `.github\workflows\ci.yml`
  change the image loop to `for target in auth message-svc gateway worker smoke load; do`.
  This is contributor tooling only; consumers need no GitHub service.

- [ ] **Step 3: Verify and commit.**

```powershell
docker build -q -f deploy\docker\Dockerfile --target load -t push-messenger/load:dev .
docker run --rm push-messenger/load:dev; "exit=$LASTEXITCODE"
git add deploy\docker\Dockerfile .github\workflows\ci.yml
git commit -m "build: add the push-load image target" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

Expected: the image builds; running it prints the usage and `exit=2`.

## Task 10: Consumer build, package and verification guide

- [ ] **Step 1: Create `docs\consumer-verification.md`** with this content:

````markdown
# Building, packaging and verifying push-messenger

Commands for consumers who include push-messenger (for example as a Git submodule) and
run it with their own infrastructure. Nothing here needs GitHub Actions, GHCR, a
particular cloud or a maintainer service: run the commands in your own automation and
publish to your own registry. Chart configuration is in
[deployment-configuration.md](deployment-configuration.md).

## Build

From source (Rust 1.85 or later; Docker for the integration tests):

```text
cargo build --release --locked -p push-auth -p push-message-svc -p push-gateway -p push-worker -p push-load
cargo test --workspace --locked
```

Images, one target per binary, from the repository root (BuildKit):

| Target | Purpose |
|---|---|
| `auth`, `message-svc`, `gateway`, `worker` | The services the chart deploys |
| `load` | `push-load`, the explicitly targeted harness below |
| `smoke` | DEV ONLY end-to-end smoke test for local clusters (needs the signing key and dev mailbox) |

```text
docker build -f deploy/docker/Dockerfile --target gateway -t <your-registry>/push-messenger/gateway:<your-version> .
```

Tag and push with your own registry and versioning; record the pushed digest of
**each** image (for example from `docker buildx imagetools inspect <ref>` or your
registry) and pin it with `images.<name>.digest`. Build all service images from one
source revision and deploy them together with the chart from that revision.

## Package and render the chart

```text
helm lint deploy/helm/push-messenger --strict -f <your-values.yaml>
helm template <release> deploy/helm/push-messenger --namespace <ns> -f <your-values.yaml> > /dev/null
helm package deploy/helm/push-messenger --version <your-chart-version> --app-version <your-version>
```

With `secrets.existingSecret` the render needs no cluster and emits no Secret; a
chart-managed render contains credentials and must not reach shared logs.

## Verification tools

| Tool | Where | What it proves | Safe on production? |
|---|---|---|---|
| `pwsh deploy/helm/test-chart.ps1` | contributor machine | chart structure and guards | n/a (no cluster) |
| `deploy/minikube/test-network-policy.ps1 -Context <ctx>` | existing cluster with an enforcing CNI | internal API isolation, by live allowed/denied connections | deploys short-lived probe pods; run deliberately |
| `push-smoke` (`helm test` in dev mode) | local cluster only | the whole flow with the DEV ONLY mailbox and signing key | **no** |
| `push-load` | any target you own or are authorized to test | connections, heartbeats, message/receipt latency, reconnects | yes, with dedicated test accounts and the limits below |

## push-load

`push-load` uses only the public API. It has no default target, discovers nothing,
creates no accounts and never needs the signing key, a mailbox or disabled rate limits.
It never prints tokens, passwords or email addresses.

### Prerequisites

- Written authorization to test the target, and a maintenance or test window agreed
  with its operators.
- **Dedicated, verified test accounts** created through your normal sign-up and email
  verification, at least `connections / devices-per-account` of them (each account has
  at most 10 active devices). Never use real users' accounts.
- A private directory outside every Git working tree for the accounts file, state file
  and reports (encrypted where available). The state holds refresh tokens.
- For large runs, a generator host sized for the connections: raise the open-file limit
  (`ulimit -n` at least twice the connections) and keep the generator's scheduling lag
  under the reported limit. Split very large tests across several generator hosts, each
  with its own accounts subset and state file; their reports are separate.

The accounts file is JSON Lines, one `{"email": "...", "password": "..."}` per line.

### Prepare (once per campaign; resumable)

```text
push-load prepare --target https://api.example.com --authorized-target api.example.com \
  --accounts /private/accounts.jsonl --state /private/state.json \
  --devices-per-account 10 --sample-pairs 20
```

It signs each account in once (auth-svc allows 60 logins per IP per 15 minutes, so
1,000 accounts take about 4 hours from one IP; the harness waits out every
`Retry-After`), registers its devices and makes the sample pairs contacts with a direct
conversation (at most 50 pairs: invites are limited per user and IP). Interrupt and
rerun it at any time; it continues where it stopped, and it signs devices in again when
a run found their tokens refused. Exit 0 means complete; failures name the accounts
file entry and reason (`invalid_credentials`, `email_not_verified`, `too_many_devices`).

### Run

```text
push-load run --target https://api.example.com --authorized-target api.example.com \
  --state /private/state.json --connections 10000 --ramp-per-second 100 \
  --duration-seconds 1200 --target-build "gateway@sha256:..., chart 1.2.3" \
  --report /private/report-1.json
```

It refreshes every selected device's token once, opens one connection per device at the
ramp rate, keeps each alive with application Pings at the announced heartbeat, and sends
a sample message every `--message-interval-seconds` (default 10) on each pair. Ramp plus
duration is limited to 25 minutes (tokens last 30 minutes; the harness does not
re-authenticate). At the end, or on Ctrl-C, it closes every socket and writes the report.
Exit 0 only when the run `passed`; 1 `failed`; 130 `interrupted`; 2 a usage or
precondition error. A run that did not pass is never a capacity result.

`--connect-to <ip>` dials another address (for example one load-balancer node) while
keeping the target's host for `Host`, TLS SNI and certificate checks.

### Reconnects during a gateway rollout

To measure reconnect behaviour, start a run with a duration that covers the rollout and,
once the ramp has finished, roll the gateway with your normal process (for example
`kubectl rollout restart deployment/<release>-gateway`). Connections closed with 4005
(shutting down) or dropped reconnect with backoff and jitter; the report counts
disconnects and reconnects and gives reconnect latency. Messages stored during the
rollout must still be delivered: `lost` must stay 0.

### Reading the report

- `status` and `failures`: `passed` requires every connection to have reached Ready and
  all to have been open at once, no connect/reconnect failure, no authentication
  failure, no takeover (4007), no rate limiting, every Ping answered within 10 s, no
  unexpected close code, no lost sample message and an unsaturated generator.
- `latency`: `connect`, `ping_rtt`, `delivery`, `receipt` and `reconnect` as count, p50,
  p90, p99 and max in milliseconds; `percentiles` states the nearest-rank definition.
- `generator.scheduling_lag`: if `saturated`, the generator limited the run and the
  results do not describe the target.
- `workload`, `target`, `target_build`, `started_unix_s` and `elapsed_s` identify what
  was measured. Keep reports with the deployment's own monitoring for the same window.

The harness proves what one run measured. Capacity claims need the consumer's real
deployment, its load balancer and monitoring, repeated runs and their own acceptance
criteria. When the campaign ends, revoke the test devices through your normal account
processes and delete the state file.

### Local functional check

On a local minikube installation (`deploy/minikube/up.ps1`), DEV ONLY accounts come
from `push-dev-accounts` in the `push-smoke` crate, which reads the worker's DEV ONLY
mailbox:

```text
cargo run -p push-smoke --bin push-dev-accounts -- --auth http://127.0.0.1:18080 \
  --host push.local --mailbox http://127.0.0.1:18081 --count 6 --out /private/accounts.jsonl
```

(with `kubectl port-forward` of the ingress controller to 18080 and of the worker to
18081). Then prepare and run against `http://push.local:18080` with
`--connect-to 127.0.0.1`.
````

- [ ] **Step 2: Link it.** In `README.md`, extend the "Deploying it" section with:

```markdown
[docs/consumer-verification.md](docs/consumer-verification.md) gives the build, image,
chart packaging and verification commands, and `push-load`, an explicitly targeted,
bounded harness for connection, heartbeat, message-latency and reconnect checks with
dedicated test accounts.
```

- [ ] **Step 3: Commit.**

```powershell
git add docs\consumer-verification.md README.md
git commit -m "docs: document consumer builds, packaging and verification" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 11: Small real deployment with a gateway rollout

Use the owned profile `push11b-local` (stopped with data retained by Plan 11B). Never
use the shared `push` profile or the `push11a*` profiles; never run `down` to recover.

- [ ] **Step 1: Bring the owned profile up with this branch's images.**

```powershell
.\deploy\minikube\up.ps1 -Cluster push11b-local
if ($LASTEXITCODE -ne 0) { throw 'push11b-local bring-up failed' }
```

- [ ] **Step 2: Prepare DEV ONLY accounts in a private directory.**

```powershell
$Private = Join-Path $env:LOCALAPPDATA ('push11c-private\' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force $Private | Out-Null
icacls $Private /inheritance:r /grant:r "$($env:USERNAME):(OI)(CI)F" | Out-Null
$ingress = Start-Process kubectl -NoNewWindow -PassThru -ArgumentList @(
    '--context', 'push11b-local', '-n', 'ingress-nginx', 'port-forward',
    'svc/ingress-nginx-controller', '18080:80', '--address', '127.0.0.1')
$mailbox = Start-Process kubectl -NoNewWindow -PassThru -ArgumentList @(
    '--context', 'push11b-local', '-n', 'push', 'port-forward', 'svc/push-worker',
    '18081:8080', '--address', '127.0.0.1')
try {
    Start-Sleep -Seconds 3
    cargo run -q -p push-smoke --bin push-dev-accounts --locked -- --auth http://127.0.0.1:18080 `
        --host push.local --mailbox http://127.0.0.1:18081 --count 6 --out (Join-Path $Private 'accounts.jsonl')
    if ($LASTEXITCODE -ne 0) { throw 'DEV ONLY accounts failed' }
    cargo run -q -p push-load --locked -- prepare --target http://push.local:18080 `
        --authorized-target push.local:18080 --connect-to 127.0.0.1 `
        --accounts (Join-Path $Private 'accounts.jsonl') --state (Join-Path $Private 'state.json') `
        --devices-per-account 2 --sample-pairs 2
    if ($LASTEXITCODE -ne 0) { throw 'prepare did not complete' }
} finally {
    $mailbox.Kill()
}
```

Expected: `prepared 12 of 12 devices and 2 of 2 sample pairs for 6 accounts`. Keep
`$ingress` running for step 3. ingress-nginx matches `Host: push.local:18080` against
the `push.local` rule; the token audience comes from the chart, not the Host header.

- [ ] **Step 3: Run with a deliberate gateway rollout in the middle.**

```powershell
$build = (kubectl --context push11b-local -n push get deployment push-gateway -o json |
    ConvertFrom-Json).spec.template.metadata.annotations.'push-messenger/build-id'
$load = Start-Process cargo -NoNewWindow -PassThru -ArgumentList @(
    'run', '-q', '-p', 'push-load', '--locked', '--', 'run', '--target', 'http://push.local:18080',
    '--authorized-target', 'push.local:18080', '--connect-to', '127.0.0.1',
    '--state', (Join-Path $Private 'state.json'), '--connections', '12', '--ramp-per-second', '4',
    '--duration-seconds', '180', '--message-interval-seconds', '5',
    '--target-build', "minikube push11b-local $build", '--report', (Join-Path $Private 'rollout.json'))
Start-Sleep -Seconds 60
kubectl --context push11b-local -n push rollout restart deployment/push-gateway
kubectl --context push11b-local -n push rollout status deployment/push-gateway --timeout=180s
$load.WaitForExit()
"push-load exit=$($load.ExitCode)"
$ingress.Kill()
$report = Get-Content (Join-Path $Private 'rollout.json') -Raw | ConvertFrom-Json
$report | Select-Object status, failures | Format-List
$report.connections | Select-Object ready_once, peak_active, disconnects, reconnects, reconnect_failures
$report.close_codes; $report.messages; $report.latency.reconnect
```

Expected: exit 0 and `status` `passed`; every connection reconnected once (close code
4005, or 1006 if the ingress drops it), `reconnect_failures` 0, `lost` 0, reconnect
latency recorded. If the run fails, read `failures`; do not relax thresholds. Fix a
harness defect in code with a regression test, or record a genuine service finding.

- [ ] **Step 4: Record the evidence** in a "Local functional evidence (2026-10-17)" section at the
  end of `docs\consumer-verification.md`: source revision, the commands above, the
  status, connections/reconnects, close codes, message counts and latency summaries
  from the report. Do not include the state file, accounts or Private path. Stop the
  profile with data retained and restore the shared context:

```powershell
minikube stop -p push11b-local
kubectl config use-context push
minikube profile push
git add docs\consumer-verification.md
git commit -m "docs: record push-load evidence on a local deployment" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

## Task 12: Final acceptance

- [ ] **Step 1: Run the affected suites.**

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p push-load -p push-smoke --locked
pwsh .\deploy\helm\test-chart.ps1
```

The MSRV job builds the workspace with Rust 1.85: avoid newer APIs (no let chains).

- [ ] **Step 2: Check each requirement against its proof.**

| Requirement (spec section 7/8) | Proof |
|---|---|
| Documented build, Dockerfile targets, chart packaging/rendering, external configuration, verification commands; no registry or pipeline required | `docs\consumer-verification.md`; Task 9 image |
| Production tools need no signing key, mailbox or disabled limits | `push-load` uses only login/devices/refresh and the socket; the DEV ONLY account helper lives in `push-smoke` |
| Explicit endpoint and authorization acknowledgement; no default, discovery or account generation | Task 1 guard tests; Task 7 CLI test |
| Explicit, resumable, bounded, rate-limit-respecting preparation | Task 5 code; Task 8 fresh/resumed/repaired preparation |
| Credentials out of source control, output and reports | Git-tree refusal and redaction tests (Task 2); report checked for tokens, passwords and emails (Task 8) |
| Bounded ramp, finite duration, timeouts, cleanup on completion or interruption | Task 1 limits; Task 6 cancellation and stop grace |
| 10,000 independent devices without takeover | one device per connection, 4007 fails the run; 20,000-device ceiling |
| Successes/failures, active connections, heartbeats, message/receipt latency, reconnects during rollout | report fields; Task 8 run; Task 11 rollout |
| Percentile definition, workload, duration, target build, generator saturation reported | report `percentiles`, `workload`, `target_build`, `generator` |
| Failed or incomplete runs never reported as success | Task 3 judgement tests; Task 8 refused-token run |
| Targeting guards, bounded execution, cleanup, auth-failure reporting, result correctness on local fixtures and a small real deployment | Tasks 1, 3, 6, 8 and 11 |

- [ ] **Step 3: Update status and commit.** Roadmap row 11C: "Implemented / under review
  (not Done until merged)" with links to this plan and `docs\consumer-verification.md`;
  the design's status line likewise. After merge, set Done with the PR number.

```powershell
git diff --check main...HEAD
git add docs\superpowers
git commit -m "docs: record 11C acceptance status" -m "Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>"
```

The 10,000-connection run, resource sizing and capacity acceptance remain the later
consumer deployment's work; nothing in this plan claims them.

## Self-review

- Spec section 7 and the 11C part of section 8 map to the table in Task 12; sections 1,
  2 and 9 constrain every task (no registry, pipeline, cloud or capacity claim).
- Types are consistent across tasks: `Target`, `RunOptions`, `PrepareOptions`
  (Task 1); `Account`, `State`, `DeviceRef`, `Pair` (Task 2); `Samples`, `Summary`,
  `Messages`, `Counters`, `CloseCodes` (Task 3); `Report`, `Status`, `judge` (Task 3);
  `AuthApi`, `ApiError`, `DeviceTokens` (Task 4); `socket::open/ask/send/close`
  (Task 4); `prepare::prepare` returning `PrepareSummary` (Task 5); `run::run`
  returning `Report` (Task 6), used unchanged by the CLI (Task 7) and tests (Task 8).
- The only deliberately compressed step is Task 8's move of the existing whole-stack
  setup into `Stack::start`: it is the current code moved verbatim with the two stated
  changes, not new behaviour.
