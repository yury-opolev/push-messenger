//! The whole system on this machine: Postgres and Redis in containers (Docker must be
//! running), auth-svc, the worker, message-svc and the gateway in this process, wired as
//! the chart wires them (each internal API behind its own token), and a router serving
//! both public APIs from one origin as an ingress does. The smoke test and push-load run
//! against them as clients see them.

mod common;

use std::time::Duration;

use common::{Containers, Stack};
use push_load::config::{CallsOptions, PrepareOptions, RunOptions, Target as LoadTarget};
use push_load::report::Status;
use push_load::state::State;
use push_smoke::run;

/// The stack on password-mode datastores in containers.
async fn start() -> (Stack, Containers) {
    let (datastores, containers) = common::password_datastores().await;
    (Stack::start(datastores).await, containers)
}

#[tokio::test]
async fn the_smoke_test_passes_against_the_whole_stack() {
    let (stack, _containers) = start().await;
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

/// push-load against the whole stack: DEV ONLY accounts; a preparation with one unusable
/// account that resumes without new devices; a passing run with sample messages and no
/// credential in its report; a refused refresh token failing a run and repaired by the
/// next preparation; and too many connections refused before any request.
#[tokio::test]
async fn push_load_prepares_runs_and_reports_against_the_whole_stack() {
    let (stack, _containers) = start().await;
    let mut accounts = push_smoke::create_verified_accounts(&stack.smoke_target(String::new()), 4)
        .await
        .expect("four verified accounts");
    accounts.push(push_load::accounts::Account::password(
        format!("nobody.{}@load.test", uuid::Uuid::now_v7().simple()),
        "not-an-account".to_owned(),
    ));
    // Credential files outside the repository: push-load refuses a Git working tree.
    let dir = std::env::temp_dir().join(format!("push-load-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let state_path = dir.join("state.json");
    let authority = stack.public_url.trim_start_matches("http://").to_owned();
    let target = LoadTarget::new(&stack.public_url, &authority, None).unwrap();
    let options = PrepareOptions {
        devices_per_account: 2,
        sample_pairs: 1,
        concurrency: 2,
    };

    let first = push_load::prepare::prepare(&target, &accounts, &state_path, &options)
        .await
        .unwrap();
    assert_eq!(
        (first.devices_ready, first.devices_wanted, first.pairs_ready),
        (8, 10, 1)
    );
    assert_eq!(first.failures.len(), 1, "{:?}", first.failures);
    assert!(
        first.failures[0].contains("entry 5") && first.failures[0].contains("invalid_credentials")
    );
    assert!(!first.complete());
    let devices = |state: &State| -> Vec<Option<uuid::Uuid>> {
        state
            .accounts
            .iter()
            .flat_map(|account| account.devices.iter().map(|device| device.device_id))
            .collect()
    };
    let prepared = State::load(&state_path).unwrap().unwrap();
    let again = push_load::prepare::prepare(&target, &accounts, &state_path, &options)
        .await
        .unwrap();
    assert_eq!((again.devices_ready, again.pairs_ready), (8, 1));
    assert_eq!(
        devices(&State::load(&state_path).unwrap().unwrap()),
        devices(&prepared),
        "resuming adds no devices"
    );

    let run_options = RunOptions {
        connections: 8,
        ramp_per_second: 8,
        duration: Duration::from_secs(35),
        message_interval: Duration::from_secs(2),
        target_build: "local whole-stack test".to_owned(),
        reconnect: true,
    };
    let report = push_load::run::run(&target, &state_path, &run_options)
        .await
        .unwrap();
    assert_eq!(report.status, Status::Passed, "{:?}", report.failures);
    assert_eq!(report.connections.ready_once, 8);
    assert!(
        report.connections.pongs >= 8,
        "a Pong per connection after one 30 s heartbeat"
    );
    assert!(
        report.messages.delivered >= 5 && report.messages.lost == 0,
        "{:?}",
        report.messages
    );
    assert!(report.latency.receipt.count >= 1);
    let json = serde_json::to_string(&report).unwrap();
    let state = State::load(&state_path).unwrap().unwrap();
    for account in &accounts {
        assert!(!json.contains(&account.email) && !json.contains(&account.password));
    }
    for device in state.accounts.iter().flat_map(|account| &account.devices) {
        if let Some(token) = &device.refresh_token {
            assert!(
                !json.contains(token.as_str()),
                "the report holds a refresh token"
            );
        }
    }

    // A refused refresh token: counted as an authentication failure, never a pass.
    let mut broken = state.clone();
    let victim = broken.ready_devices()[4];
    broken.device_mut(victim).refresh_token = Some("not-a-refresh-token".to_owned());
    broken.save(&state_path).unwrap();
    let short = RunOptions {
        duration: Duration::from_secs(10),
        ..run_options.clone()
    };
    let failed = push_load::run::run(&target, &state_path, &short)
        .await
        .unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert_eq!(
        (
            failed.connections.auth_failures,
            failed.connections.ready_once
        ),
        (1, 7)
    );
    assert!(
        State::load(&state_path)
            .unwrap()
            .unwrap()
            .device(victim)
            .needs_sign_in
    );
    let repaired = push_load::prepare::prepare(&target, &accounts, &state_path, &options)
        .await
        .unwrap();
    assert_eq!(repaired.devices_ready, 8);
    assert_eq!(
        devices(&State::load(&state_path).unwrap().unwrap()),
        devices(&prepared),
        "the same device signed in again"
    );

    let too_many = RunOptions {
        connections: 9,
        ..run_options
    };
    let refused = push_load::run::run(&target, &state_path, &too_many)
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("prepared devices"), "{refused}");

    stack.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

/// Prepares `pairs` sample pairs of fresh verified accounts (one device each) in a new
/// directory outside the repository; the state path, the accounts and the directory.
async fn prepare_pairs(
    stack: &Stack,
    pairs: usize,
) -> (
    LoadTarget,
    std::path::PathBuf,
    Vec<push_load::accounts::Account>,
    std::path::PathBuf,
) {
    let accounts =
        push_smoke::create_verified_accounts(&stack.smoke_target(String::new()), pairs * 2)
            .await
            .expect("verified accounts");
    let dir = std::env::temp_dir().join(format!("push-load-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let state_path = dir.join("state.json");
    let authority = stack.public_url.trim_start_matches("http://").to_owned();
    let target = LoadTarget::new(&stack.public_url, &authority, None).unwrap();
    let options = PrepareOptions {
        devices_per_account: 1,
        sample_pairs: pairs,
        concurrency: 2,
    };
    let prepared = push_load::prepare::prepare(&target, &accounts, &state_path, &options)
        .await
        .unwrap();
    assert!(prepared.complete(), "{:?}", prepared.failures);
    (target, state_path, accounts, dir)
}

fn calls_options(calls: usize, duration_s: u64, hold_s: u64) -> CallsOptions {
    CallsOptions {
        calls,
        ramp_per_second: 4,
        duration: Duration::from_secs(duration_s),
        hold: Duration::from_secs(hold_s),
        attempts_per_hour: 30,
        // A debug build of the whole stack on one machine: the 300 ms target is for a
        // deployment, not this test.
        setup_p95: Duration::from_secs(5),
        target_build: "local whole-stack test".to_owned(),
        redis: None,
    }
}

/// A Redis user that may only run `INFO`, as an operator would give push-load: the URL
/// of `redis_url` (no password) with that user and a password.
async fn redis_stats_url(redis_url: &str) -> String {
    let client = redis::Client::open(redis_url).unwrap();
    let mut connection = client.get_multiplexed_async_connection().await.unwrap();
    redis::cmd("ACL")
        .arg(&["SETUSER", "push-load-stats", "on", ">stats-s3cret", "+info"])
        .query_async::<()>(&mut connection)
        .await
        .unwrap();
    redis_url.replace("redis://", "redis://push-load-stats:stats-s3cret@")
}

/// push-load's calls scenario against the whole stack with calls enabled: two pairs
/// place, answer, hold and hang up calls for 20 s; the report passes, records setup
/// latency and the Redis commands the calls cost, and holds no credential.
#[tokio::test]
async fn push_load_calls_place_and_complete_calls_against_the_whole_stack() {
    let (datastores, _containers) = common::calls_datastores().await;
    let redis_url = common::lookup(datastores.gateway.clone())("PUSH_REDIS_URL").unwrap();
    let stats_url = redis_stats_url(&redis_url).await;
    let stack = Stack::start(datastores).await;
    let (target, state_path, accounts, dir) = prepare_pairs(&stack, 2).await;

    let options = CallsOptions {
        redis: Some(push_load::commandstats::RedisUrl::new(&stats_url).unwrap()),
        ..calls_options(2, 20, 2)
    };
    let report = push_load::calls::run(&target, &state_path, &options)
        .await
        .unwrap();
    assert_eq!(report.status, Status::Passed, "{:?}", report.failures);
    let calls = &report.calls;
    assert_eq!(report.connections.ready_once, 4);
    assert!(calls.completed >= 2, "{calls:?}");
    assert_eq!(
        (calls.placed, calls.accepted, calls.completed),
        (calls.completed, calls.completed, calls.completed),
        "{calls:?}"
    );
    assert!(
        calls.errors.is_empty() && calls.unexpected_frames == 0,
        "{calls:?}"
    );
    assert_eq!(calls.call_syncs, 4);
    let completed = std::collections::BTreeMap::from([("completed".to_owned(), calls.completed)]);
    assert_eq!(
        (&calls.ended, &calls.callee_ended),
        (&completed, &completed),
        "both parties heard every call end COMPLETED"
    );
    assert_eq!(calls.ice_received, calls.ice_sent, "{calls:?}");
    assert_eq!(calls.peak_active, 2, "both pairs were in a call at once");
    assert_eq!(report.latency.setup.count as u64, calls.accepted);
    assert!(report.latency.setup.p95_ms.is_some());
    let redis = report.redis.as_ref().expect("Redis deltas");
    assert!(redis.error.is_none() && !redis.reset, "{redis:?}");
    assert!(
        redis
            .commands
            .get("evalsha")
            .is_some_and(|evalsha| evalsha.calls > 0),
        "the call engine's scripts: {redis:?}"
    );
    assert!(redis.total_calls > 0 && redis.calls_per_call_placed.is_some());

    let json = serde_json::to_string(&report).unwrap();
    assert!(
        !json.contains("stats-s3cret") && !json.contains(&stats_url),
        "the report holds the Redis URL"
    );
    for account in &accounts {
        assert!(!json.contains(&account.email) && !json.contains(&account.password));
    }
    let state = State::load(&state_path).unwrap().unwrap();
    for device in state.accounts.iter().flat_map(|account| &account.devices) {
        if let Some(token) = &device.refresh_token {
            assert!(
                !json.contains(token.as_str()),
                "the report holds a refresh token"
            );
        }
    }
    assert!(!json.contains("m=audio"), "the report holds SDP");

    let too_many = push_load::calls::run(&target, &state_path, &calls_options(3, 20, 2))
        .await
        .unwrap_err()
        .to_string();
    assert!(too_many.contains("sample pairs"), "{too_many}");

    stack.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}

/// A calls run against a target without calls fails at once, naming the reason.
#[tokio::test]
async fn push_load_calls_fail_clearly_when_the_target_offers_no_calls() {
    let (stack, _containers) = start().await;
    let (target, state_path, _accounts, dir) = prepare_pairs(&stack, 1).await;
    let started = std::time::Instant::now();
    let report = push_load::calls::run(&target, &state_path, &calls_options(1, 60, 2))
        .await
        .unwrap();
    assert_eq!(report.status, Status::Failed);
    assert!(
        report.failures[0].contains("calls are not enabled on the target"),
        "{:?}",
        report.failures
    );
    assert_eq!(report.calls.placed, 0);
    assert!(
        started.elapsed() < Duration::from_secs(40),
        "the run ends early"
    );
    stack.stop().await;
    std::fs::remove_dir_all(dir).unwrap();
}
