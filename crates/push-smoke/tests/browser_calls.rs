//! The browser smoke test of voice calls (docs/calls.md, "Browser smoke test"): the whole
//! stack in this process with calls enabled, two verified contacts, the example page
//! (docs/examples/calls) behind its own server (`serve.mjs`), and Playwright driving two
//! Chromium contexts with fake media devices that call each other. Two TURN modes:
//! `stun-only` (direct candidates) and `internal` with relay-only ICE through the chart's
//! pinned coturn in Docker.
//!
//! Opt in with `PUSH_BROWSER_CALLS=1` (otherwise the tests pass without running): it needs
//! Docker, Node.js, `npm ci` and `npx playwright install chromium` in
//! docs/examples/calls/tests. CI runs it in the `browser-calls` job.

mod common;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::{Containers, Datastores, Stack};
use push_load::config::{PrepareOptions, Target as LoadTarget};
use push_load::http::AuthApi;
use push_load::state::State;

const TURN_SECRET: &str = "the-coturn-shared-secret-of-the-browser-test";

fn enabled() -> bool {
    if std::env::var("PUSH_BROWSER_CALLS").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("skipped: set PUSH_BROWSER_CALLS=1 to run the browser smoke test");
    false
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn example_dir() -> PathBuf {
    repo().join("docs/examples/calls")
}

/// A child process killed when the test ends, passed or not.
struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The stack with calls enabled and auth-svc's TURN settings `turn`.
async fn stack(turn: &[(&str, &str)]) -> (Stack, Containers) {
    let (mut datastores, containers): (Datastores, Containers) = common::calls_datastores().await;
    datastores.auth.extend(
        turn.iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    (Stack::start(datastores).await, containers)
}

/// Two verified accounts made contacts by push-load's `prepare`, one device each: their
/// user ids and access tokens.
async fn contacts(stack: &Stack) -> [(String, String); 2] {
    let accounts = push_smoke::create_verified_accounts(&stack.smoke_target(String::new()), 2)
        .await
        .expect("two verified accounts");
    // Credential files outside the repository: push-load refuses a Git working tree.
    let dir = std::env::temp_dir().join(format!("push-browser-calls-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let state_path = dir.join("state.json");
    let authority = stack.public_url.trim_start_matches("http://").to_owned();
    let target = LoadTarget::new(&stack.public_url, &authority, None).unwrap();
    let options = PrepareOptions {
        devices_per_account: 1,
        sample_pairs: 1,
        concurrency: 1,
    };
    let prepared = push_load::prepare::prepare(&target, &accounts, &state_path, &options)
        .await
        .unwrap();
    assert!(prepared.complete(), "{:?}", prepared.failures);
    let state = State::load(&state_path).unwrap().expect("the state file");
    let api = AuthApi::new(&target).unwrap();
    let mut out = Vec::new();
    for account in &state.accounts {
        let refresh = account.devices[0]
            .refresh_token
            .clone()
            .expect("a ready device");
        let tokens = api.refresh(&refresh).await.expect("an access token");
        out.push((tokens.user_id.to_string(), tokens.access_token));
    }
    let _ = std::fs::remove_dir_all(dir);
    out.try_into().expect("two accounts")
}

/// serve.mjs in front of the stack: the page's origin.
fn serve(stack: &Stack) -> (Killed, String) {
    let mut child = Command::new("node")
        .arg(example_dir().join("serve.mjs"))
        .args(["--api", &stack.public_url, "--port", "0"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("start serve.mjs (is Node.js installed?)");
    let stdout = child.stdout.take().unwrap();
    let child = Killed(child);
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let url = line
        .split_whitespace()
        .find(|word| word.starts_with("http://"))
        .unwrap_or_else(|| panic!("serve.mjs did not start: {line:?}"))
        .to_owned();
    (child, url)
}

/// Runs the Playwright suite against `page_url`; panics unless every test passed.
async fn playwright(page_url: String, a: (String, String), b: (String, String), relay: bool) {
    let tests = example_dir().join("tests");
    let cli = tests.join("node_modules/@playwright/test/cli.js");
    assert!(
        cli.exists(),
        "run `npm ci` and `npx playwright install chromium` in {}",
        tests.display()
    );
    // Killed when the caller's timeout drops this future.
    let status = tokio::process::Command::new("node")
        .arg(cli)
        .arg("test")
        .current_dir(&tests)
        .env("CALLS_BASE_URL", page_url)
        .env("CALLS_A_USER", a.0)
        .env("CALLS_A_TOKEN", a.1)
        .env("CALLS_B_USER", b.0)
        .env("CALLS_B_TOKEN", b.1)
        .env("CALLS_RELAY", if relay { "1" } else { "0" })
        .kill_on_drop(true)
        .status()
        .await
        .expect("run Playwright");
    assert!(status.success(), "the Playwright calls suite failed");
}

#[tokio::test]
async fn browsers_call_each_other_with_stun_only() {
    if !enabled() {
        return;
    }
    // No STUN server is reachable here: host candidates connect the two local browsers.
    let (stack, _containers) = stack(&[
        ("PUSH_TURN_MODE", "stun-only"),
        ("PUSH_TURN_STUN_URLS", "stun:127.0.0.1:3478"),
    ])
    .await;
    let [a, b] = contacts(&stack).await;
    let (_serve, url) = serve(&stack);
    tokio::time::timeout(Duration::from_secs(300), playwright(url, a, b, false))
        .await
        .expect("the browser suite finishes within 300 s");
    stack.stop().await;
}

#[tokio::test]
async fn browsers_call_each_other_through_the_internal_turn_relay() {
    if !enabled() {
        return;
    }
    let coturn = common::coturn::start(TURN_SECRET).await;
    let urls = format!("turn:127.0.0.1:{}?transport=tcp", coturn.tcp_port);
    let (stack, _containers) = stack(&[
        ("PUSH_TURN_MODE", "internal"),
        ("PUSH_TURN_URLS", &urls),
        ("PUSH_TURN_SHARED_SECRET", TURN_SECRET),
    ])
    .await;
    let [a, b] = contacts(&stack).await;
    let (_serve, url) = serve(&stack);
    tokio::time::timeout(Duration::from_secs(300), playwright(url, a, b, true))
        .await
        .expect("the browser suite finishes within 300 s");
    stack.stop().await;
}
