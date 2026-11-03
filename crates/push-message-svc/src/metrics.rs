//! The call engine's metrics in the Prometheus text format 0.0.4 (spec §11, D17),
//! served on the health listener at `GET /metrics`. Hand-rolled on atomics: no metrics
//! dependency. Labels are reasons, codes and frame kinds only, never call content.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use push_domain::calls::EndReason;
use push_proto::v1::ErrorCode;

/// The upper bounds of the histogram buckets, in seconds (`+Inf` follows).
const BUCKETS: [f64; 9] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5];

const END_REASONS: [EndReason; 7] = [
    EndReason::Completed,
    EndReason::Missed,
    EndReason::Declined,
    EndReason::Cancelled,
    EndReason::Busy,
    EndReason::Failed,
    EndReason::Unreachable,
];

/// The refusal codes counted, with their label.
const REFUSAL_CODES: [(ErrorCode, &str); 7] = [
    (ErrorCode::NotContact, "not_contact"),
    (ErrorCode::CallNotFound, "call_not_found"),
    (ErrorCode::Invalid, "invalid"),
    (ErrorCode::AlreadyAnswered, "already_answered"),
    (ErrorCode::InCall, "in_call"),
    (ErrorCode::CallLimit, "call_limit"),
    (ErrorCode::Unavailable, "unavailable"),
];

/// A client call frame, the `frame` label of `push_call_signal_seconds`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignalFrame {
    Invite,
    Accept,
    Decline,
    Cancel,
    Ice,
    Hangup,
    Renegotiate,
}

impl SignalFrame {
    const ALL: [SignalFrame; 7] = [
        Self::Invite,
        Self::Accept,
        Self::Decline,
        Self::Cancel,
        Self::Ice,
        Self::Hangup,
        Self::Renegotiate,
    ];

    /// The label value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invite => "invite",
            Self::Accept => "accept",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
            Self::Ice => "ice",
            Self::Hangup => "hangup",
            Self::Renegotiate => "renegotiate",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// A histogram with the fixed [`BUCKETS`].
#[derive(Debug, Default)]
struct Histogram {
    /// Per bucket (not cumulative); the last one is `+Inf`.
    counts: [AtomicU64; BUCKETS.len() + 1],
    sum_micros: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    fn observe(&self, value: Duration) {
        let seconds = value.as_secs_f64();
        let bucket = BUCKETS
            .iter()
            .position(|bound| seconds <= *bound)
            .unwrap_or(BUCKETS.len());
        self.counts[bucket].fetch_add(1, Ordering::Relaxed);
        let micros = u64::try_from(value.as_micros()).unwrap_or(u64::MAX);
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// The `_bucket`, `_sum` and `_count` lines, with `labels` (`frame="ice",` or empty).
    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let mut cumulative = 0;
        for (index, count) in self.counts.iter().enumerate() {
            cumulative += count.load(Ordering::Relaxed);
            let bound = BUCKETS
                .get(index)
                .map_or_else(|| "+Inf".to_owned(), |bound| bound.to_string());
            let _ = writeln!(out, "{name}_bucket{{{labels}le=\"{bound}\"}} {cumulative}");
        }
        let sum = self.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        let braces = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{}}}", labels.trim_end_matches(','))
        };
        let _ = writeln!(out, "{name}_sum{braces} {sum}");
        let _ = writeln!(
            out,
            "{name}_count{braces} {}",
            self.count.load(Ordering::Relaxed)
        );
    }
}

/// The call engine's counters and histograms.
#[derive(Debug, Default)]
pub struct Metrics {
    started: AtomicU64,
    answered: AtomicU64,
    ended: [AtomicU64; END_REASONS.len()],
    refused: [AtomicU64; REFUSAL_CODES.len()],
    setup: Histogram,
    signal: [Histogram; SignalFrame::ALL.len()],
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// A call started ringing.
    pub fn call_started(&self) {
        self.started.fetch_add(1, Ordering::Relaxed);
    }

    /// A call was answered, `setup` after its accept was received.
    pub fn call_answered(&self, setup: Duration) {
        self.answered.fetch_add(1, Ordering::Relaxed);
        self.setup.observe(setup);
    }

    /// A call ended (or could not ring) for `reason`.
    pub fn call_ended(&self, reason: EndReason) {
        let index = END_REASONS
            .iter()
            .position(|known| *known == reason)
            .unwrap_or_default();
        self.ended[index].fetch_add(1, Ordering::Relaxed);
    }

    /// A call frame was refused with `code`; codes the engine never answers are ignored.
    pub fn frame_refused(&self, code: ErrorCode) {
        if let Some(index) = REFUSAL_CODES.iter().position(|(known, _)| *known == code) {
            self.refused[index].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A call frame was handled and its frames published in `elapsed`.
    pub fn frame_handled(&self, frame: SignalFrame, elapsed: Duration) {
        self.signal[frame.index()].observe(elapsed);
    }

    /// Every family, with `push_calls_live` when `live` is known.
    pub fn render(&self, live: Option<u64>) -> String {
        let mut out = String::new();
        counter(
            &mut out,
            "push_calls_started_total",
            "Calls that started ringing.",
        );
        let _ = writeln!(
            out,
            "push_calls_started_total {}",
            self.started.load(Ordering::Relaxed)
        );
        counter(&mut out, "push_calls_answered_total", "Calls answered.");
        let _ = writeln!(
            out,
            "push_calls_answered_total {}",
            self.answered.load(Ordering::Relaxed)
        );
        counter(
            &mut out,
            "push_calls_ended_total",
            "Calls ended, and invites that could not ring, by reason.",
        );
        for (reason, count) in END_REASONS.iter().zip(&self.ended) {
            let _ = writeln!(
                out,
                "push_calls_ended_total{{reason=\"{}\"}} {}",
                reason.as_str(),
                count.load(Ordering::Relaxed)
            );
        }
        counter(
            &mut out,
            "push_call_frames_refused_total",
            "Call frames refused, by error code.",
        );
        for ((_, label), count) in REFUSAL_CODES.iter().zip(&self.refused) {
            let _ = writeln!(
                out,
                "push_call_frames_refused_total{{code=\"{label}\"}} {}",
                count.load(Ordering::Relaxed)
            );
        }
        histogram(
            &mut out,
            "push_call_setup_seconds",
            "From an accept received to CallAccepted published.",
        );
        self.setup.render(&mut out, "push_call_setup_seconds", "");
        histogram(
            &mut out,
            "push_call_signal_seconds",
            "Time to handle a call frame and publish its frames, by frame.",
        );
        for (frame, histogram) in SignalFrame::ALL.iter().zip(&self.signal) {
            let labels = format!("frame=\"{}\",", frame.as_str());
            histogram.render(&mut out, "push_call_signal_seconds", &labels);
        }
        if let Some(live) = live {
            let _ = writeln!(
                out,
                "# HELP push_calls_live Calls ringing or active.\n# TYPE push_calls_live gauge\npush_calls_live {live}"
            );
        }
        out
    }
}

fn counter(out: &mut String, name: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
}

fn histogram(out: &mut String, name: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} histogram");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_is_rendered_once_with_its_type() {
        let metrics = Metrics::new();
        let text = metrics.render(Some(3));
        for (name, kind) in [
            ("push_calls_started_total", "counter"),
            ("push_calls_answered_total", "counter"),
            ("push_calls_ended_total", "counter"),
            ("push_call_frames_refused_total", "counter"),
            ("push_call_setup_seconds", "histogram"),
            ("push_call_signal_seconds", "histogram"),
            ("push_calls_live", "gauge"),
        ] {
            assert_eq!(
                text.matches(&format!("# TYPE {name} {kind}\n")).count(),
                1,
                "{name}"
            );
            assert_eq!(text.matches(&format!("# HELP {name} ")).count(), 1);
        }
        assert!(text.contains("push_calls_live 3\n"));
        for reason in END_REASONS {
            let line = format!(
                "push_calls_ended_total{{reason=\"{}\"}} 0\n",
                reason.as_str()
            );
            assert!(text.contains(&line), "{line}");
        }
        assert!(text.contains("push_call_frames_refused_total{code=\"not_contact\"} 0\n"));
        assert!(text.contains("push_call_signal_seconds_count{frame=\"renegotiate\"} 0\n"));
        assert!(!metrics.render(None).contains("push_calls_live"));
    }

    #[test]
    fn buckets_are_cumulative() {
        let metrics = Metrics::new();
        for millis in [3, 7, 300, 4_000] {
            metrics.frame_handled(SignalFrame::Ice, Duration::from_millis(millis));
        }
        metrics.call_answered(Duration::from_millis(40));
        metrics.call_ended(EndReason::Missed);
        metrics.frame_refused(ErrorCode::CallLimit);
        metrics.frame_refused(ErrorCode::BadFrame);
        metrics.call_started();
        let text = metrics.render(None);
        let ice =
            |le: &str| format!("push_call_signal_seconds_bucket{{frame=\"ice\",le=\"{le}\"}}");
        for (le, count) in [
            ("0.005", 1),
            ("0.01", 2),
            ("0.25", 2),
            ("0.5", 3),
            ("2.5", 3),
            ("+Inf", 4),
        ] {
            assert!(text.contains(&format!("{} {count}\n", ice(le))), "{le}");
        }
        assert!(text.contains("push_call_signal_seconds_count{frame=\"ice\"} 4\n"));
        assert!(text.contains("push_call_signal_seconds_sum{frame=\"ice\"} 4.31\n"));
        assert!(text.contains("push_call_setup_seconds_bucket{le=\"0.025\"} 0\n"));
        assert!(text.contains("push_call_setup_seconds_bucket{le=\"0.05\"} 1\n"));
        assert!(text.contains("push_call_setup_seconds_count 1\n"));
        assert!(text.contains("push_calls_answered_total 1\n"));
        assert!(text.contains("push_calls_started_total 1\n"));
        assert!(text.contains("push_calls_ended_total{reason=\"missed\"} 1\n"));
        assert!(text.contains("push_call_frames_refused_total{code=\"call_limit\"} 1\n"));
        assert!(!text.contains("bad_frame"));
    }
}
