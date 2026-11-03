//! The report of a calls run and its judgement, in the style of a run's report: `passed`
//! only when every criterion holds. It names the target and build and never holds
//! tokens, passwords, email addresses or SDP.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::metrics::{Summary, nearest_rank};
use crate::report::{Connections, Generator, Status};

/// How every percentile in a calls report is defined.
pub const CALL_PERCENTILES: &str = "Nearest rank over every sample of the run, in milliseconds: pN is the smallest sample with at least N% of the samples at or below it; max is the largest sample. Setup latency runs from sending CallInvite to the caller's CallAccepted; the callee answers CallIncoming at once, so it holds no human response time. Connect latency runs from a connection's first attempt to Ready.";

#[derive(Debug, Clone, Serialize)]
pub struct CallsReport {
    pub tool: String,
    pub tool_version: String,
    pub scenario: String,
    pub status: Status,
    pub failures: Vec<String>,
    pub target: String,
    pub target_build: String,
    pub started_unix_s: u64,
    pub elapsed_s: f64,
    pub workload: CallsWorkload,
    pub connections: Connections,
    /// Close codes of connections that had reached Ready.
    pub close_codes: BTreeMap<String, u64>,
    /// Close codes answering Identify before Ready.
    pub ready_refusals: BTreeMap<String, u64>,
    pub calls: CallCounts,
    pub latency: CallLatency,
    pub generator: Generator,
    pub percentiles: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallsWorkload {
    /// Concurrent calls asked for: one per sample pair.
    pub calls: usize,
    pub connections: usize,
    pub ramp_per_second: u32,
    pub ramp_s: f64,
    pub duration_s: u64,
    pub hold_s: u64,
    pub attempts_per_hour: u32,
    /// The most invites one pair could send in the run.
    pub attempts_per_pair_max: u64,
    pub setup_p95_limit_ms: u64,
    pub heartbeat_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CallCounts {
    /// `CallInvite` frames sent.
    pub placed: u64,
    /// Answered by `CallAccepted`.
    pub accepted: u64,
    /// Accepted and ended `COMPLETED`.
    pub completed: u64,
    /// Not accepted within 10 s of the invite (the caller then hung up).
    pub unanswered: u64,
    /// A party heard no `CallEnded` within 10 s of the hangup, or before the run ended.
    pub endings_missing: u64,
    /// `CallEnded` reasons the callers heard, by name.
    pub ended: BTreeMap<String, u64>,
    /// `CallEnded` reasons the callees heard, by name (none for a call that never rang).
    pub callee_ended: BTreeMap<String, u64>,
    /// `CallError` frames, by code.
    pub errors: BTreeMap<String, u64>,
    /// Call frames that fit no call of the pair.
    pub unexpected_frames: u64,
    pub ice_sent: u64,
    /// `peer_call_ice` frames received.
    pub ice_received: u64,
    pub call_syncs: u64,
    /// Calls left over from an earlier run, listed by `CallSync` and hung up.
    pub stale_calls: u64,
    /// Connections whose Ready did not list `CAPABILITY_CALLS`.
    pub not_offered: u64,
    pub peak_active: i64,
    pub peak_ringing_or_active: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CallLatency {
    pub setup: SetupSummary,
    pub connect: Summary,
    pub ping_rtt: Summary,
}

/// The setup latency's percentiles, with the p95 its target names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SetupSummary {
    pub count: usize,
    pub p50_ms: Option<u32>,
    pub p95_ms: Option<u32>,
    pub p99_ms: Option<u32>,
    pub max_ms: Option<u32>,
}

impl SetupSummary {
    /// `sorted` ascending.
    pub fn of(sorted: &[u32]) -> Self {
        if sorted.is_empty() {
            return Self::default();
        }
        Self {
            count: sorted.len(),
            p50_ms: Some(nearest_rank(sorted, 50)),
            p95_ms: Some(nearest_rank(sorted, 95)),
            p99_ms: Some(nearest_rank(sorted, 99)),
            max_ms: sorted.last().copied(),
        }
    }
}

fn listed(counts: &BTreeMap<String, u64>) -> String {
    counts
        .iter()
        .map(|(name, count)| format!("{name} x{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The status and the reasons it is not `Passed`.
pub fn judge(report: &CallsReport, interrupted: bool) -> (Status, Vec<String>) {
    let mut failures = Vec::new();
    let connections = &report.connections;
    let calls = &report.calls;
    let requested = report.workload.connections as u64;
    if calls.not_offered > 0 {
        failures.push(format!(
            "calls are not enabled on the target: {} connections' Ready did not list CAPABILITY_CALLS (set calls.enabled)",
            calls.not_offered
        ));
    }
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
        (
            connections.connect_failures,
            "connections failed to connect within 60 s",
        ),
        (
            connections.auth_failures,
            "devices failed authentication (refresh refused, or 4001, 4002 or 4003)",
        ),
        (
            connections.takeovers,
            "connections were taken over by another connection of the same device (4007)",
        ),
        (
            connections.rate_limited,
            "connections were rate limited (4004 or RATE_LIMITED)",
        ),
        (connections.pong_timeouts, "Pings got no Pong within 10 s"),
        (
            connections.protocol_failures,
            "frames from the gateway could not be decoded",
        ),
        (
            connections.error_frames,
            "Error frames from the gateway (a refused request)",
        ),
        (
            connections.disconnects,
            "connections dropped during the run (a calls run does not reconnect)",
        ),
        (
            calls.unanswered,
            "calls were not accepted within 10 s of the invite",
        ),
        (
            calls.endings_missing,
            "calls did not end for both parties within 10 s of the hangup or before the run ended",
        ),
        (
            calls.unexpected_frames,
            "call frames fitted no call of their pair",
        ),
    ] {
        if count > 0 {
            failures.push(format!("{count} {what}"));
        }
    }
    let ready = connections.ready_once.saturating_sub(calls.not_offered);
    if calls.call_syncs < ready {
        failures.push(format!(
            "{} connections got no CallSync after Ready",
            ready - calls.call_syncs
        ));
    }
    if calls.not_offered == 0 && calls.placed == 0 {
        failures.push("no calls were placed".to_owned());
    }
    if calls.placed > 0
        && u64::try_from(calls.peak_active).unwrap_or(0) < report.workload.calls as u64
    {
        failures.push(format!(
            "at most {} of {} calls were active at once",
            calls.peak_active, report.workload.calls
        ));
    }
    if !calls.errors.is_empty() {
        failures.push(format!("CallError frames: {}", listed(&calls.errors)));
    }
    for (endings, party) in [(&calls.ended, "callers"), (&calls.callee_ended, "callees")] {
        let other: BTreeMap<String, u64> = endings
            .iter()
            .filter(|(reason, _)| reason.as_str() != "completed")
            .map(|(reason, count)| (reason.clone(), *count))
            .collect();
        if !other.is_empty() {
            failures.push(format!(
                "calls ended other than COMPLETED for {party}: {}",
                listed(&other)
            ));
        }
    }
    if let Some(p95) = report.latency.setup.p95_ms {
        if u64::from(p95) > report.workload.setup_p95_limit_ms {
            failures.push(format!(
                "call setup p95 {p95} ms is above {} ms",
                report.workload.setup_p95_limit_ms
            ));
        }
    }
    if report.generator.saturated {
        failures.push(format!(
            "the load generator was saturated (p99 scheduling lag {} ms > {} ms): the results do not describe the target",
            report.generator.scheduling_lag.p99_ms.unwrap_or(0),
            report.generator.lag_limit_ms
        ));
    }
    let status = if interrupted {
        failures.insert(
            0,
            "the run was interrupted before its duration ended".to_owned(),
        );
        Status::Interrupted
    } else if failures.is_empty() {
        Status::Passed
    } else {
        Status::Failed
    };
    (status, failures)
}

fn millis(value: Option<u32>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

/// A short human summary; the JSON report holds everything.
pub fn summary_text(report: &CallsReport) -> String {
    let status = match report.status {
        Status::Passed => "PASSED",
        Status::Failed => "FAILED",
        Status::Interrupted => "INTERRUPTED",
    };
    let c = &report.connections;
    let calls = &report.calls;
    let setup = &report.latency.setup;
    let mut text = format!(
        "push-load calls: {status} against {} (build {})\n",
        report.target, report.target_build
    );
    let _ = writeln!(
        text,
        "  connections: {}/{} ready, peak {} open, {} disconnects",
        c.ready_once, report.workload.connections, c.peak_active, c.disconnects
    );
    let _ = writeln!(
        text,
        "  calls: placed {}, accepted {}, completed {}, peak {} active and {} ringing or active",
        calls.placed,
        calls.accepted,
        calls.completed,
        calls.peak_active,
        calls.peak_ringing_or_active
    );
    let _ = writeln!(
        text,
        "  setup ms: p50 {}, p95 {} (limit {}), p99 {}, max {}",
        millis(setup.p50_ms),
        millis(setup.p95_ms),
        report.workload.setup_p95_limit_ms,
        millis(setup.p99_ms),
        millis(setup.max_ms)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn passing() -> CallsReport {
        CallsReport {
            tool: "push-load".to_owned(),
            tool_version: "0.1.0".to_owned(),
            scenario: "calls".to_owned(),
            status: Status::Failed,
            failures: Vec::new(),
            target: "https://api.example.com".to_owned(),
            target_build: "test".to_owned(),
            started_unix_s: 0,
            elapsed_s: 60.0,
            workload: CallsWorkload {
                calls: 2,
                connections: 4,
                ramp_per_second: 4,
                ramp_s: 1.0,
                duration_s: 30,
                hold_s: 2,
                attempts_per_hour: 30,
                attempts_per_pair_max: 7,
                setup_p95_limit_ms: 300,
                heartbeat_ms: Some(30_000),
            },
            connections: Connections {
                ready_once: 4,
                peak_active: 4,
                ..Connections::default()
            },
            close_codes: BTreeMap::new(),
            ready_refusals: BTreeMap::new(),
            calls: CallCounts {
                placed: 10,
                accepted: 10,
                completed: 10,
                ended: BTreeMap::from([("completed".to_owned(), 10)]),
                callee_ended: BTreeMap::from([("completed".to_owned(), 10)]),
                call_syncs: 4,
                ice_sent: 40,
                ice_received: 40,
                peak_active: 2,
                peak_ringing_or_active: 2,
                ..CallCounts::default()
            },
            latency: CallLatency {
                setup: SetupSummary::of(&[20, 30, 40, 50, 60, 70, 80, 90, 100, 110]),
                ..CallLatency::default()
            },
            generator: Generator {
                scheduling_lag: Summary::default(),
                lag_limit_ms: 250,
                saturated: false,
            },
            percentiles: String::new(),
        }
    }

    #[test]
    fn setup_percentiles_include_the_p95() {
        let values: Vec<u32> = (1..=100).collect();
        assert_eq!(
            SetupSummary::of(&values),
            SetupSummary {
                count: 100,
                p50_ms: Some(50),
                p95_ms: Some(95),
                p99_ms: Some(99),
                max_ms: Some(100)
            }
        );
        assert_eq!(SetupSummary::of(&[]), SetupSummary::default());
    }

    #[test]
    fn a_clean_calls_run_passes() {
        assert_eq!(judge(&passing(), false), (Status::Passed, Vec::new()));
    }

    /// A change that must fail the run, and a phrase its failure must contain.
    type Case = (&'static str, Box<dyn Fn(&mut CallsReport)>);

    #[test]
    fn every_calls_criterion_fails_the_run() {
        let cases: Vec<Case> = vec![
            (
                "calls are not enabled on the target",
                Box::new(|r| r.calls.not_offered = 4),
            ),
            (
                "never became ready",
                Box::new(|r| r.connections.ready_once = 3),
            ),
            ("open at once", Box::new(|r| r.connections.peak_active = 3)),
            (
                "failed to connect",
                Box::new(|r| r.connections.connect_failures = 1),
            ),
            (
                "authentication",
                Box::new(|r| r.connections.auth_failures = 1),
            ),
            ("dropped", Box::new(|r| r.connections.disconnects = 1)),
            ("no Pong", Box::new(|r| r.connections.pong_timeouts = 1)),
            ("Error frames", Box::new(|r| r.connections.error_frames = 1)),
            ("no CallSync", Box::new(|r| r.calls.call_syncs = 3)),
            ("no calls were placed", Box::new(|r| r.calls.placed = 0)),
            (
                "CallError frames: in_call x1, rate_limited x2",
                Box::new(|r| {
                    r.calls.errors.insert("rate_limited".to_owned(), 2);
                    r.calls.errors.insert("in_call".to_owned(), 1);
                }),
            ),
            (
                "calls ended other than COMPLETED for callers: busy x1",
                Box::new(|r| {
                    r.calls.ended.insert("busy".to_owned(), 1);
                }),
            ),
            (
                "calls ended other than COMPLETED for callees: cancelled x1",
                Box::new(|r| {
                    r.calls.callee_ended.insert("cancelled".to_owned(), 1);
                }),
            ),
            (
                "at most 1 of 2 calls were active at once",
                Box::new(|r| r.calls.peak_active = 1),
            ),
            (
                "not accepted within 10 s",
                Box::new(|r| r.calls.unanswered = 1),
            ),
            (
                "did not end for both parties",
                Box::new(|r| r.calls.endings_missing = 1),
            ),
            (
                "fitted no call",
                Box::new(|r| r.calls.unexpected_frames = 1),
            ),
            (
                "call setup p95 110 ms is above 100 ms",
                Box::new(|r| r.workload.setup_p95_limit_ms = 100),
            ),
            ("saturated", Box::new(|r| r.generator.saturated = true)),
        ];
        for (expected, change) in cases {
            let mut report = passing();
            change(&mut report);
            let (status, failures) = judge(&report, false);
            assert_eq!(status, Status::Failed, "{expected}");
            assert!(
                failures.iter().any(|failure| failure.contains(expected)),
                "{expected}: {failures:?}"
            );
        }
    }

    #[test]
    fn an_interrupted_calls_run_never_passes() {
        let (status, failures) = judge(&passing(), true);
        assert_eq!(status, Status::Interrupted);
        assert!(failures[0].contains("interrupted"));
    }

    #[test]
    fn the_calls_summary_names_status_and_setup() {
        let mut report = passing();
        report.status = Status::Passed;
        let text = summary_text(&report);
        assert!(
            text.contains("PASSED") && text.contains("p95 110 (limit 300)"),
            "{text}"
        );
    }
}
