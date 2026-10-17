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
        (
            connections.connect_failures,
            "connections failed to connect within 60 s",
        ),
        (
            connections.reconnect_failures,
            "connections failed to reconnect within 60 s",
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
        failures.push(format!(
            "{} stored sample messages were never delivered",
            messages.lost
        ));
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
        pair(&l.connect),
        pair(&l.ping_rtt),
        pair(&l.delivery),
        pair(&l.receipt),
        pair(&l.reconnect)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MessageCounts, Summary};

    fn passing() -> Report {
        let lag = Summary {
            count: 100,
            p50_ms: Some(1),
            p90_ms: Some(2),
            p99_ms: Some(5),
            max_ms: Some(9),
        };
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
            connections: Connections {
                ready_once: 4,
                peak_active: 4,
                ..Connections::default()
            },
            close_codes: BTreeMap::new(),
            ready_refusals: BTreeMap::new(),
            messages: MessageCounts {
                sent: 5,
                stored: 5,
                delivered: 5,
                receipted: 5,
                ..MessageCounts::default()
            },
            latency: Latency::default(),
            generator: Generator {
                scheduling_lag: lag,
                lag_limit_ms: 250,
                saturated: false,
            },
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

    /// A change that must fail the run, and a phrase its failure must contain.
    type Case = (&'static str, Box<dyn Fn(&mut Report)>);

    #[test]
    fn every_criterion_fails_the_run() {
        let cases: Vec<Case> = vec![
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
                "failed to reconnect",
                Box::new(|r| r.connections.reconnect_failures = 1),
            ),
            (
                "authentication",
                Box::new(|r| r.connections.auth_failures = 1),
            ),
            ("taken over", Box::new(|r| r.connections.takeovers = 1)),
            ("rate limited", Box::new(|r| r.connections.rate_limited = 1)),
            ("no Pong", Box::new(|r| r.connections.pong_timeouts = 1)),
            ("decoded", Box::new(|r| r.connections.protocol_failures = 1)),
            (
                "unexpected close codes: 1008x2",
                Box::new(|r| {
                    r.close_codes.insert("1008".to_owned(), 2);
                }),
            ),
            ("never delivered", Box::new(|r| r.messages.lost = 1)),
            (
                "no sample messages",
                Box::new(|r| r.messages = MessageCounts::default()),
            ),
            ("saturated", Box::new(|r| r.generator.saturated = true)),
        ];
        for (expected, change) in cases {
            let mut report = passing();
            change(&mut report);
            let (status, failures) = judged(&report, false);
            assert_eq!(status, Status::Failed, "{expected}");
            assert!(
                failures.iter().any(|failure| failure.contains(expected)),
                "{expected}: {failures:?}"
            );
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
        assert!(
            text.contains("PASSED")
                && text.contains("https://api.example.com")
                && text.contains("test"),
            "{text}"
        );
    }
}
