//! When the bridge calls its owner (`calls.notify_by_call`): a request left unanswered for
//! a while, or urgent news; never more often than the minimum interval, never in the
//! do-not-disturb hours. Pure: the core passes the times in.
//!
//! Do-not-disturb ranges are written `"22:00-07:00"` (every day), `"mon-fri 22:00-07:00"`
//! or `"sat,sun 00:00-24:00"`, in the computer's local time. A range whose end is not
//! after its start runs past midnight, into the next day; its days are the days it
//! starts on.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveDateTime, TimeZone, Timelike, Utc};
use tokio::time::Instant;

use crate::config::AnswerMode;

/// Minutes in a day.
const DAY: u16 = 24 * 60;
const DAY_NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// How the bridge answers and places calls.
#[derive(Clone, Default)]
pub struct CallPolicy {
    /// How the owner's calls are answered.
    pub answer: AnswerMode,
    /// When the bridge calls the owner; off by default.
    pub notify: NotifyPolicy,
    /// The local time now; `None` uses the computer's clock and time zone.
    pub local_time: Option<LocalTime>,
}

/// The local wall-clock time now (tests inject one).
pub type LocalTime = Arc<dyn Fn() -> NaiveDateTime + Send + Sync>;

impl CallPolicy {
    /// The local time now.
    pub fn local_now(&self) -> NaiveDateTime {
        match &self.local_time {
            Some(now) => now(),
            None => local(Utc::now(), &chrono::Local),
        }
    }
}

/// `utc` as a wall-clock time in `zone`.
pub fn local<Tz: TimeZone>(utc: DateTime<Utc>, zone: &Tz) -> NaiveDateTime {
    utc.with_timezone(zone).naive_local()
}

/// When the bridge calls its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyPolicy {
    /// A permission request, question or plan unanswered this long; `None`: never.
    pub after: Option<Duration>,
    /// Calls for urgent background messages.
    pub urgent: bool,
    /// At most one call per this.
    pub min_interval: Duration,
    /// No call starts in these hours.
    pub quiet: QuietHours,
}

impl Default for NotifyPolicy {
    fn default() -> Self {
        Self {
            after: None,
            urgent: false,
            min_interval: Duration::from_secs(10 * 60),
            quiet: QuietHours::default(),
        }
    }
}

/// Whether a call may start now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Call,
    /// The last call was less than the minimum interval ago.
    TooSoon,
    /// Do-not-disturb hours.
    DoNotDisturb,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Call => "call",
            Verdict::TooSoon => "too_soon",
            Verdict::DoNotDisturb => "do_not_disturb",
        }
    }
}

impl NotifyPolicy {
    /// Whether anything ever starts a call.
    pub fn enabled(&self) -> bool {
        self.after.is_some() || self.urgent
    }

    /// Whether a request opened at `opened` waited long enough at `now`.
    pub fn overdue(&self, opened: Instant, now: Instant) -> bool {
        self.after
            .is_some_and(|after| now.saturating_duration_since(opened) >= after)
    }

    /// Whether a call may start at `now` (local time `local`), the last one having
    /// started at `last`.
    pub fn verdict(&self, now: Instant, last: Option<Instant>, local: NaiveDateTime) -> Verdict {
        if self.quiet.contains(local) {
            return Verdict::DoNotDisturb;
        }
        if last.is_some_and(|last| now.saturating_duration_since(last) < self.min_interval) {
            return Verdict::TooSoon;
        }
        Verdict::Call
    }
}

/// Do-not-disturb hours: any of the ranges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuietHours {
    ranges: Vec<QuietRange>,
}

/// One range: on `days` (Monday first) from `start` for minutes until `end` (minutes
/// after midnight); past midnight when `end <= start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QuietRange {
    days: [bool; 7],
    start: u16,
    end: u16,
}

impl QuietHours {
    /// Parses the ranges as written; the error names `at[i]` and says how to write it.
    pub fn parse(ranges: &[String], at: &str) -> Result<Self, String> {
        let ranges = ranges
            .iter()
            .enumerate()
            .map(|(i, text)| {
                QuietRange::parse(text).map_err(|why| {
                    format!(
                        "{at}[{i}] {why}: write \"22:00-07:00\", \"mon-fri 22:00-07:00\" or \"sat,sun 00:00-24:00\""
                    )
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { ranges })
    }

    /// Whether `local` lies in a range.
    pub fn contains(&self, local: NaiveDateTime) -> bool {
        let day = local.weekday().num_days_from_monday() as usize;
        let minute = u16::try_from(local.hour() * 60 + local.minute()).unwrap_or(0);
        self.ranges.iter().any(|range| range.contains(day, minute))
    }
}

impl QuietRange {
    fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim().to_ascii_lowercase();
        let (days, times) = match text.split_once(char::is_whitespace) {
            Some((days, times)) => (parse_days(days)?, times.trim()),
            None => ([true; 7], text.as_str()),
        };
        let (start, end) = times
            .split_once('-')
            .ok_or_else(|| "needs a time range".to_owned())?;
        let start = parse_time(start, false)?;
        let end = parse_time(end, true)?;
        if start == end {
            return Err("is empty (its start and end are the same)".to_owned());
        }
        Ok(Self { days, start, end })
    }

    fn contains(&self, day: usize, minute: u16) -> bool {
        if self.start < self.end {
            return self.days[day] && (self.start..self.end).contains(&minute);
        }
        let yesterday = (day + 6) % 7;
        (self.days[day] && minute >= self.start) || (self.days[yesterday] && minute < self.end)
    }
}

/// `mon-fri`, `sat,sun`, `mon,wed-fri`; a range may wrap (`fri-mon`).
fn parse_days(text: &str) -> Result<[bool; 7], String> {
    let mut days = [false; 7];
    for part in text.split(',') {
        let day = |name: &str| {
            DAY_NAMES
                .iter()
                .position(|day| *day == name.trim())
                .ok_or_else(|| format!("has an unknown day {:?}", name.trim()))
        };
        match part.split_once('-') {
            Some((first, last)) => {
                let (first, last) = (day(first)?, day(last)?);
                let mut current = first;
                loop {
                    days[current] = true;
                    if current == last {
                        break;
                    }
                    current = (current + 1) % 7;
                }
            }
            None => days[day(part)?] = true,
        }
    }
    Ok(days)
}

/// `HH:MM` as minutes after midnight; `24:00` only as an end.
fn parse_time(text: &str, end: bool) -> Result<u16, String> {
    let bad = || format!("has a bad time {:?}", text.trim());
    let (hours, minutes) = text.trim().split_once(':').ok_or_else(bad)?;
    if hours.len() != 2 || minutes.len() != 2 {
        return Err(bad());
    }
    let hours: u16 = hours.parse().map_err(|_| bad())?;
    let minutes: u16 = minutes.parse().map_err(|_| bad())?;
    match (hours, minutes) {
        (24, 0) if end => Ok(DAY),
        (0..=23, 0..=59) => Ok(hours * 60 + minutes),
        _ => Err(bad()),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, NaiveDate};

    use super::*;

    /// 2026-11-02 is a Monday.
    fn at(day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, day)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    fn quiet(ranges: &[&str]) -> QuietHours {
        let ranges: Vec<String> = ranges.iter().map(|range| (*range).to_owned()).collect();
        QuietHours::parse(&ranges, "dnd").unwrap()
    }

    #[test]
    fn a_range_past_midnight_covers_both_days() {
        let hours = quiet(&["22:00-07:00"]);
        assert!(hours.contains(at(2, 22, 0)));
        assert!(hours.contains(at(2, 23, 59)));
        assert!(hours.contains(at(3, 0, 0)));
        assert!(hours.contains(at(3, 6, 59)));
        assert!(!hours.contains(at(3, 7, 0)));
        assert!(!hours.contains(at(2, 21, 59)));
        assert!(!hours.contains(at(2, 12, 0)));
    }

    #[test]
    fn days_are_the_days_a_range_starts_on() {
        let hours = quiet(&["mon-fri 22:00-07:00"]);
        // Friday night into Saturday morning.
        assert!(hours.contains(at(6, 23, 0)));
        assert!(hours.contains(at(7, 6, 0)));
        // Saturday night is free, and so is Sunday's.
        assert!(!hours.contains(at(7, 23, 0)));
        assert!(!hours.contains(at(8, 23, 0)));
        // Sunday night's range is Monday's early morning: Sunday is not listed.
        assert!(!hours.contains(at(2, 6, 0)));
        // Monday night into Tuesday.
        assert!(hours.contains(at(3, 6, 0)));
    }

    #[test]
    fn whole_days_lists_and_wrapping_day_ranges_parse() {
        let weekend = quiet(&["sat,sun 00:00-24:00"]);
        assert!(weekend.contains(at(7, 0, 0)));
        assert!(weekend.contains(at(8, 23, 59)));
        assert!(!weekend.contains(at(9, 0, 0)));
        assert!(!weekend.contains(at(6, 23, 59)));
        let wrapping = quiet(&["fri-mon 12:00-13:00"]);
        for day in [6, 7, 8, 9] {
            assert!(wrapping.contains(at(day, 12, 30)), "{day}");
        }
        assert!(!wrapping.contains(at(10, 12, 30)));
        let several = quiet(&["mon,wed 09:00-10:00", "12:00-12:30"]);
        assert!(several.contains(at(4, 9, 30)));
        assert!(!several.contains(at(3, 9, 30)));
        assert!(several.contains(at(3, 12, 10)));
        assert!(quiet(&["00:00-24:00"]).contains(at(5, 13, 0)));
        assert!(!QuietHours::default().contains(at(5, 13, 0)));
    }

    #[test]
    fn bad_ranges_are_refused_with_their_index() {
        for (text, why) in [
            ("22-07", "bad time"),
            ("22:00", "needs a time range"),
            ("24:00-07:00", "bad time"),
            ("22:60-07:00", "bad time"),
            ("7:00-08:00", "bad time"),
            ("08:00-08:00", "is empty"),
            ("weekdays 08:00-09:00", "unknown day"),
            ("mon-xyz 08:00-09:00", "unknown day"),
        ] {
            let ranges = vec!["23:00-06:00".to_owned(), text.to_owned()];
            let error =
                QuietHours::parse(&ranges, "calls.notify_by_call.do_not_disturb").unwrap_err();
            assert!(
                error.starts_with("calls.notify_by_call.do_not_disturb[1] "),
                "{text}: {error}"
            );
            assert!(error.contains(why), "{text}: {error}");
        }
    }

    #[test]
    fn local_time_follows_the_zone() {
        let utc = Utc.with_ymd_and_hms(2026, 11, 2, 21, 30, 0).unwrap();
        let hours = quiet(&["22:00-07:00"]);
        // 21:30 UTC is 22:30 in Copenhagen in winter (UTC+1): quiet there.
        let cet = FixedOffset::east_opt(3600).unwrap();
        assert_eq!(local(utc, &cet), at(2, 22, 30));
        assert!(hours.contains(local(utc, &cet)));
        // ... and 16:30 in New York (UTC-5): not quiet.
        let est = FixedOffset::west_opt(5 * 3600).unwrap();
        assert_eq!(local(utc, &est), at(2, 16, 30));
        assert!(!hours.contains(local(utc, &est)));
        // East of UTC the date moves on: Tuesday 06:30 in Tokyo, still quiet.
        let jst = FixedOffset::east_opt(9 * 3600).unwrap();
        assert_eq!(local(utc, &jst), at(3, 6, 30));
        assert!(hours.contains(local(utc, &jst)));
        // The weekday follows the zone too.
        let weekday_nights = quiet(&["mon 22:00-23:00"]);
        assert!(weekday_nights.contains(local(utc, &FixedOffset::east_opt(3600).unwrap())));
        assert!(!weekday_nights.contains(local(utc, &FixedOffset::east_opt(-3600 * 10).unwrap())));
    }

    #[tokio::test(start_paused = true)]
    async fn the_verdict_checks_the_quiet_hours_then_the_interval() {
        let policy = NotifyPolicy {
            after: Some(Duration::from_secs(300)),
            urgent: false,
            min_interval: Duration::from_secs(600),
            quiet: quiet(&["22:00-07:00"]),
        };
        let now = Instant::now();
        let day = at(2, 12, 0);
        assert_eq!(policy.verdict(now, None, day), Verdict::Call);
        assert_eq!(
            policy.verdict(now, None, at(2, 23, 0)),
            Verdict::DoNotDisturb
        );
        tokio::time::advance(Duration::from_secs(599)).await;
        assert_eq!(
            policy.verdict(Instant::now(), Some(now), day),
            Verdict::TooSoon
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            policy.verdict(Instant::now(), Some(now), day),
            Verdict::Call
        );
        assert_eq!(
            policy.verdict(Instant::now(), Some(now), at(3, 6, 0)),
            Verdict::DoNotDisturb
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_is_overdue_after_the_wait_and_never_when_off() {
        let opened = Instant::now();
        let policy = NotifyPolicy {
            after: Some(Duration::from_secs(120)),
            ..NotifyPolicy::default()
        };
        assert!(policy.enabled());
        assert!(!policy.overdue(opened, Instant::now()));
        tokio::time::advance(Duration::from_secs(120)).await;
        assert!(policy.overdue(opened, Instant::now()));
        let off = NotifyPolicy::default();
        assert!(!off.enabled());
        assert!(!off.overdue(opened, Instant::now()));
        let urgent_only = NotifyPolicy {
            urgent: true,
            ..NotifyPolicy::default()
        };
        assert!(urgent_only.enabled());
        assert!(!urgent_only.overdue(opened, Instant::now()));
    }
}
