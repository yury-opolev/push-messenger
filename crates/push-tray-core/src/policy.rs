//! When the tray restarts a bridge that exited: exponential backoff, and giving up after
//! too many exits in a short time.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The first delay.
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);
/// The longest delay.
pub const BACKOFF_CAP: Duration = Duration::from_secs(60);
/// How long an exit counts towards giving up.
pub const WINDOW: Duration = Duration::from_secs(10 * 60);
/// The tray gives up at this many exits within [`WINDOW`].
pub const MAX_EXITS: usize = 5;
/// A run at least this long starts the backoff over.
pub const RESET_AFTER: Duration = Duration::from_secs(60);
/// The bridge's exit code for a missing setup step (no key, not enrolled).
pub const EXIT_NOT_SET_UP: i32 = 2;

/// What to do after a bridge exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Start it again after the delay.
    Restart(Duration),
    /// Too many exits: leave it stopped.
    GiveUp,
    /// It exited with code 2: it needs keygen and enroll; never retried.
    NotSetUp,
    /// The tray asked it to stop.
    Stopped,
    /// It exited with code 0 without the tray asking: it was stopped on purpose elsewhere
    /// (Ctrl-C, or `shutdown` from another tool on the control channel). Never restarted,
    /// never counted.
    StoppedElsewhere,
}

/// One bridge's restart policy. Call [`RestartPolicy::reset`] when the owner starts it.
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    backoff_base: Duration,
    cap: Duration,
    window: Duration,
    max_exits: usize,
    reset_after: Duration,
    exits: VecDeque<Instant>,
    next: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl RestartPolicy {
    pub fn new() -> Self {
        Self {
            backoff_base: BACKOFF_BASE,
            cap: BACKOFF_CAP,
            window: WINDOW,
            max_exits: MAX_EXITS,
            reset_after: RESET_AFTER,
            exits: VecDeque::new(),
            next: BACKOFF_BASE,
        }
    }

    /// Decides after an exit with `code` (`None`: killed by a signal) at `now`, after a run
    /// of `ran_for`. A requested stop, code 0 (the bridge was stopped on purpose elsewhere)
    /// and code 2 are not counted as failures.
    pub fn on_exit(
        &mut self,
        code: Option<i32>,
        requested_stop: bool,
        ran_for: Duration,
        now: Instant,
    ) -> Decision {
        if requested_stop {
            return Decision::Stopped;
        }
        if code == Some(0) {
            return Decision::StoppedElsewhere;
        }
        if code == Some(EXIT_NOT_SET_UP) {
            return Decision::NotSetUp;
        }
        while let Some(oldest) = self.exits.front() {
            if now.saturating_duration_since(*oldest) >= self.window {
                self.exits.pop_front();
            } else {
                break;
            }
        }
        self.exits.push_back(now);
        if self.exits.len() >= self.max_exits {
            return Decision::GiveUp;
        }
        if ran_for >= self.reset_after {
            self.next = self.backoff_base;
        }
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(self.cap);
        Decision::Restart(delay)
    }

    /// Forgets the exits and starts the backoff over.
    pub fn reset(&mut self) {
        self.exits.clear();
        self.next = self.backoff_base;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUICK: Duration = Duration::from_secs(1);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn the_backoff_doubles_up_to_a_minute() {
        let mut policy = RestartPolicy::new();
        let start = Instant::now();
        let mut delays = Vec::new();
        // An exit every 3 minutes: never 5 within 10 minutes.
        for n in 0..10 {
            match policy.on_exit(Some(1), false, QUICK, start + secs(180 * n)) {
                Decision::Restart(delay) => delays.push(delay.as_secs()),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
    }

    #[test]
    fn five_quick_exits_give_up_on_the_fifth() {
        let mut policy = RestartPolicy::new();
        let start = Instant::now();
        for n in 0..4 {
            assert!(matches!(
                policy.on_exit(Some(1), false, QUICK, start + secs(n)),
                Decision::Restart(_)
            ));
        }
        assert_eq!(
            policy.on_exit(Some(1), false, QUICK, start + secs(4)),
            Decision::GiveUp
        );
    }

    #[test]
    fn exits_spread_over_more_than_ten_minutes_never_give_up() {
        let mut policy = RestartPolicy::new();
        let start = Instant::now();
        for n in 0..30 {
            let decision = policy.on_exit(Some(1), false, QUICK, start + secs(150 * n));
            assert!(
                matches!(decision, Decision::Restart(_)),
                "{n}: {decision:?}"
            );
        }
    }

    #[test]
    fn the_fifth_exit_within_the_window_gives_up_after_old_ones_are_forgotten() {
        let mut policy = RestartPolicy::new();
        let start = Instant::now();
        policy.on_exit(Some(1), false, QUICK, start);
        for n in 0..3 {
            policy.on_exit(Some(1), false, QUICK, start + secs(500 + n));
        }
        // The first exit is now 10 minutes old: forgotten, 4 in the window.
        assert!(matches!(
            policy.on_exit(Some(1), false, QUICK, start + WINDOW),
            Decision::Restart(_)
        ));
        assert_eq!(
            policy.on_exit(Some(1), false, QUICK, start + WINDOW + secs(1)),
            Decision::GiveUp
        );
    }

    #[test]
    fn exit_code_2_is_not_set_up_and_never_counted() {
        let mut policy = RestartPolicy::new();
        let now = Instant::now();
        for _ in 0..10 {
            assert_eq!(
                policy.on_exit(Some(2), false, QUICK, now),
                Decision::NotSetUp
            );
        }
        assert_eq!(
            policy.on_exit(Some(1), false, QUICK, now),
            Decision::Restart(secs(1))
        );
    }

    #[test]
    fn a_requested_stop_is_stopped_whatever_the_code() {
        let mut policy = RestartPolicy::new();
        let now = Instant::now();
        for code in [None, Some(0), Some(1), Some(2)] {
            assert_eq!(policy.on_exit(code, true, QUICK, now), Decision::Stopped);
        }
        assert_eq!(
            policy.on_exit(Some(1), false, QUICK, now),
            Decision::Restart(secs(1))
        );
    }

    #[test]
    fn an_unrequested_exit_0_is_a_deliberate_stop_and_never_counted() {
        let mut policy = RestartPolicy::new();
        let now = Instant::now();
        for _ in 0..10 {
            assert_eq!(
                policy.on_exit(Some(0), false, QUICK, now),
                Decision::StoppedElsewhere
            );
        }
        assert_eq!(
            policy.on_exit(Some(1), false, QUICK, now),
            Decision::Restart(secs(1))
        );
    }

    #[test]
    fn a_long_run_resets_the_backoff_but_still_counts() {
        let mut policy = RestartPolicy::new();
        let start = Instant::now();
        for (n, expected) in [1, 2, 4].into_iter().enumerate() {
            assert_eq!(
                policy.on_exit(Some(1), false, QUICK, start + secs(n as u64)),
                Decision::Restart(secs(expected))
            );
        }
        assert_eq!(
            policy.on_exit(Some(1), false, RESET_AFTER, start + secs(70)),
            Decision::Restart(secs(1))
        );
        // Four exits in the window already: the fifth gives up even after a long run.
        assert_eq!(
            policy.on_exit(Some(1), false, secs(120), start + secs(200)),
            Decision::GiveUp
        );
    }

    #[test]
    fn reset_forgets_everything() {
        let mut policy = RestartPolicy::new();
        let now = Instant::now();
        for _ in 0..4 {
            policy.on_exit(None, false, QUICK, now);
        }
        policy.reset();
        assert_eq!(
            policy.on_exit(None, false, QUICK, now),
            Decision::Restart(secs(1))
        );
    }
}
