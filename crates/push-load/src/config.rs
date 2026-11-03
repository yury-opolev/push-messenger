//! The target and every bound of a preparation or run. Nothing here touches the
//! network: the guards run before any request.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::Url;

use crate::commandstats::RedisUrl;

/// Most connections one run opens and devices one preparation keeps.
pub const MAX_CONNECTIONS: usize = 20_000;
/// auth-svc's limit of active devices per account (`MAX_ACTIVE_DEVICES`).
pub const MAX_DEVICES_PER_ACCOUNT: usize = 10;
/// Most sender/receiver pairs: enough for 1,000 concurrent calls with room to spare. The
/// gateway allows 20 redeems per user and 60 per IP an hour, so a large preparation takes
/// several runs of `prepare` (each resumes where the last stopped).
pub const MAX_SAMPLE_PAIRS: usize = 2000;
/// Most pairs a `run` sends sample messages on, whatever the preparation holds: more
/// pairs serve `calls`, not a larger message load.
pub const RUN_SAMPLE_PAIRS_MAX: usize = 50;
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
        let port = url
            .port_or_known_default()
            .context("--target has no port")?;
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
            Some(value) => Some(value.parse::<IpAddr>().context(
                "--connect-to must be an IP address such as 10.0.0.5 (the target's port is used)",
            )?),
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
            bail!(
                "--duration-seconds must be at least {}",
                MIN_DURATION.as_secs()
            );
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
        validate_build(&self.target_build)
    }
}

fn validate_build(build: &str) -> anyhow::Result<()> {
    let build = build.trim();
    if build.is_empty()
        || build.len() > MAX_BUILD_LABEL
        || !build.chars().all(|c| c.is_ascii_graphic() || c == ' ')
    {
        bail!(
            "--target-build must be 1 to {MAX_BUILD_LABEL} printable ASCII characters naming what is deployed"
        );
    }
    Ok(())
}

/// The gateway's default `calls.attemptsPerHour`.
pub const DEFAULT_CALL_ATTEMPTS_PER_HOUR: u32 = 30;
/// The gateway's largest `calls.attemptsPerHour`.
pub const MAX_CALL_ATTEMPTS_PER_HOUR: u32 = 1000;
/// The setup target within a region: p95 from `CallInvite` to `CallAccepted`.
pub const DEFAULT_SETUP_P95: Duration = Duration::from_millis(300);
pub const MAX_SETUP_P95: Duration = Duration::from_secs(10);
pub const MAX_HOLD: Duration = Duration::from_secs(20 * 60);
/// At least this long passes between a pair's call ending and its next invite.
pub const CALL_PAUSE: Duration = Duration::from_secs(1);
/// No call is placed unless its hold ends this long before the run does, so every call
/// placed can be answered (within this long) and hung up in time.
pub const CALL_MARGIN: Duration = Duration::from_secs(10);

/// The bounds of a calls run: `calls` prepared pairs, each placing one call after another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallsOptions {
    /// Concurrent calls: one per prepared pair, two connections each.
    pub calls: usize,
    /// Connections opened per second.
    pub ramp_per_second: u32,
    pub duration: Duration,
    /// How long each call stays active before the caller hangs up.
    pub hold: Duration,
    /// The target's `calls.attemptsPerHour`, as the operator states it.
    pub attempts_per_hour: u32,
    /// The run fails when the setup latency's p95 is above this.
    pub setup_p95: Duration,
    pub target_build: String,
    /// The target's Redis, for `INFO commandstats` before and after the run.
    pub redis: Option<RedisUrl>,
}

impl CallsOptions {
    pub fn connections(&self) -> usize {
        self.calls * 2
    }

    pub fn ramp_time(&self) -> Duration {
        Duration::from_secs_f64(self.connections() as f64 / f64::from(self.ramp_per_second.max(1)))
    }

    /// The shortest time from one invite of a pair to its next.
    pub fn period(&self) -> Duration {
        self.hold + CALL_PAUSE
    }

    /// The most invites one pair's caller can send in the run: one every [`period`]
    /// (at least) from its connection, the last one `hold` plus [`CALL_MARGIN`] before
    /// the run ends.
    ///
    /// [`period`]: Self::period
    pub fn attempts_per_pair(&self) -> u64 {
        let window = (self.ramp_time() + self.duration).saturating_sub(self.hold + CALL_MARGIN);
        let period = self.period().as_millis().max(1);
        u64::try_from(window.as_millis() / period).unwrap_or(u64::MAX) + 1
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !(1..=MAX_SAMPLE_PAIRS).contains(&self.calls) {
            bail!("--calls must be 1 to {MAX_SAMPLE_PAIRS}");
        }
        if !(1..=MAX_RAMP_PER_SECOND).contains(&self.ramp_per_second) {
            bail!("--ramp-per-second must be 1 to {MAX_RAMP_PER_SECOND}");
        }
        if self.duration < MIN_DURATION {
            bail!(
                "--duration-seconds must be at least {}",
                MIN_DURATION.as_secs()
            );
        }
        if self.ramp_time() + self.duration > MAX_RUN {
            bail!(
                "the ramp ({:.0} s) plus the duration must stay within {} minutes: access tokens last 30 minutes and push-load does not re-authenticate",
                self.ramp_time().as_secs_f64(),
                MAX_RUN.as_secs() / 60
            );
        }
        if !(Duration::from_secs(1)..=MAX_HOLD).contains(&self.hold) {
            bail!("--hold-seconds must be 1 to {}", MAX_HOLD.as_secs());
        }
        if self.hold + CALL_MARGIN > self.duration {
            bail!(
                "--duration-seconds must be at least --hold-seconds plus {} s, so a call can be placed and hung up in time",
                CALL_MARGIN.as_secs()
            );
        }
        if self.hold <= self.ramp_time() {
            bail!(
                "--hold-seconds must be longer than the ramp ({:.1} s: {} connections at --ramp-per-second {}), so every pair's first call is still active when the last pair's starts and all --calls {} can be active at once",
                self.ramp_time().as_secs_f64(),
                self.connections(),
                self.ramp_per_second,
                self.calls
            );
        }
        if !(1..=MAX_CALL_ATTEMPTS_PER_HOUR).contains(&self.attempts_per_hour) {
            bail!("--attempts-per-hour must be 1 to {MAX_CALL_ATTEMPTS_PER_HOUR}");
        }
        let attempts = self.attempts_per_pair();
        if attempts > u64::from(self.attempts_per_hour) {
            bail!(
                "each pair could place {attempts} calls in this run, above --attempts-per-hour {}: the gateway would refuse the rest with RATE_LIMITED. Hold calls longer (--hold-seconds), shorten the run, or raise calls.attemptsPerHour on the target (at most {MAX_CALL_ATTEMPTS_PER_HOUR}) and pass the new value",
                self.attempts_per_hour
            );
        }
        if !(Duration::from_millis(1)..=MAX_SETUP_P95).contains(&self.setup_p95) {
            bail!("--setup-p95-ms must be 1 to {}", MAX_SETUP_P95.as_millis());
        }
        validate_build(&self.target_build)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_operator_must_repeat_the_exact_target_authority() {
        assert!(Target::new("https://api.example.com", "api.example.com", None).is_ok());
        assert!(Target::new("https://api.example.com", " API.example.com ", None).is_ok());
        for wrong in [
            "",
            "example.com",
            "api.example.com:443",
            "other.example.com",
        ] {
            assert!(
                Target::new("https://api.example.com", wrong, None).is_err(),
                "{wrong:?}"
            );
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
        let target = Target::new(
            "https://api.example.com",
            "api.example.com",
            Some("10.0.0.5"),
        )
        .unwrap();
        assert_eq!(
            target.api_url("/v1/auth/login"),
            "https://api.example.com/v1/auth/login"
        );
        assert_eq!(target.socket_url(), "wss://api.example.com/v1/socket");
        assert_eq!(target.dial_address(), "10.0.0.5:443");
        let local = Target::new("http://push.local:18080", "push.local:18080", None).unwrap();
        assert_eq!(local.socket_url(), "ws://push.local:18080/v1/socket");
        assert_eq!(local.dial_address(), "push.local:18080");
        // An IP target needs no override; a malformed override is refused.
        assert!(Target::new("http://127.0.0.1:8080", "127.0.0.1:8080", Some("127.0.0.2")).is_err());
        assert!(
            Target::new(
                "https://api.example.com",
                "api.example.com",
                Some("10.0.0.5:1")
            )
            .is_err()
        );
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
            RunOptions {
                connections: 0,
                ..ok.clone()
            },
            RunOptions {
                connections: MAX_CONNECTIONS + 1,
                ..ok.clone()
            },
            RunOptions {
                ramp_per_second: 0,
                ..ok.clone()
            },
            RunOptions {
                ramp_per_second: MAX_RAMP_PER_SECOND + 1,
                ..ok.clone()
            },
            RunOptions {
                duration: Duration::from_secs(5),
                ..ok.clone()
            },
            // 100 s of ramp plus 24 minutes exceeds 25 minutes.
            RunOptions {
                duration: Duration::from_secs(24 * 60),
                ..ok.clone()
            },
            RunOptions {
                message_interval: Duration::from_millis(500),
                ..ok.clone()
            },
            RunOptions {
                target_build: String::new(),
                ..ok.clone()
            },
            RunOptions {
                target_build: "two\nlines".to_owned(),
                ..ok.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    fn calls_options() -> CallsOptions {
        CallsOptions {
            calls: 1000,
            ramp_per_second: 100,
            duration: Duration::from_secs(20 * 60),
            hold: Duration::from_secs(60),
            attempts_per_hour: DEFAULT_CALL_ATTEMPTS_PER_HOUR,
            setup_p95: DEFAULT_SETUP_P95,
            target_build: "gateway@sha256:0123".to_owned(),
            redis: None,
        }
    }

    #[test]
    fn a_full_scale_calls_run_fits_the_default_attempts_budget() {
        let ok = calls_options();
        ok.validate().unwrap();
        assert_eq!(ok.connections(), 2000);
        assert_eq!(ok.ramp_time(), Duration::from_secs(20));
        // 20 s of ramp and 1200 s of run, less 60 s of hold and 10 s of margin: 1150 s
        // at one call per 61 s is 18 periods, 19 invites.
        assert_eq!(ok.attempts_per_pair(), 19);
    }

    #[test]
    fn a_calls_run_beyond_the_attempts_budget_is_refused_up_front() {
        let short_holds = CallsOptions {
            ramp_per_second: 400,
            hold: Duration::from_secs(10),
            ..calls_options()
        };
        let error = short_holds.validate().unwrap_err().to_string();
        assert!(
            error.contains("--attempts-per-hour 30") && error.contains("calls.attemptsPerHour"),
            "{error}"
        );
        // The same run passes once the target allows more.
        CallsOptions {
            attempts_per_hour: 200,
            ..short_holds
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn a_hold_shorter_than_the_ramp_is_refused_as_the_calls_could_never_all_be_active() {
        // 2,000 connections at 100 a second ramp for 20 s.
        let short = CallsOptions {
            hold: Duration::from_secs(20),
            attempts_per_hour: MAX_CALL_ATTEMPTS_PER_HOUR,
            ..calls_options()
        };
        let error = short.validate().unwrap_err().to_string();
        assert!(
            error.contains("--hold-seconds") && error.contains("ramp"),
            "{error}"
        );
        CallsOptions {
            hold: Duration::from_secs(21),
            ..short
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn calls_runs_are_bounded() {
        let ok = calls_options();
        for bad in [
            CallsOptions {
                calls: 0,
                ..ok.clone()
            },
            CallsOptions {
                calls: MAX_SAMPLE_PAIRS + 1,
                ..ok.clone()
            },
            CallsOptions {
                ramp_per_second: 0,
                ..ok.clone()
            },
            CallsOptions {
                duration: Duration::from_secs(5),
                ..ok.clone()
            },
            CallsOptions {
                duration: Duration::from_secs(25 * 60),
                ..ok.clone()
            },
            CallsOptions {
                hold: Duration::ZERO,
                ..ok.clone()
            },
            CallsOptions {
                hold: MAX_HOLD + Duration::from_secs(1),
                ..ok.clone()
            },
            // The hold must leave the margin inside the duration.
            CallsOptions {
                duration: Duration::from_secs(30),
                hold: Duration::from_secs(25),
                ..ok.clone()
            },
            CallsOptions {
                attempts_per_hour: 0,
                ..ok.clone()
            },
            CallsOptions {
                attempts_per_hour: MAX_CALL_ATTEMPTS_PER_HOUR + 1,
                ..ok.clone()
            },
            CallsOptions {
                setup_p95: Duration::ZERO,
                ..ok.clone()
            },
            CallsOptions {
                target_build: " ".to_owned(),
                ..ok.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn preparation_is_bounded() {
        let ok = PrepareOptions {
            devices_per_account: 10,
            sample_pairs: 50,
            concurrency: 16,
        };
        ok.validate().unwrap();
        for bad in [
            PrepareOptions {
                devices_per_account: 0,
                ..ok
            },
            PrepareOptions {
                devices_per_account: 11,
                ..ok
            },
            PrepareOptions {
                sample_pairs: MAX_SAMPLE_PAIRS + 1,
                ..ok
            },
            PrepareOptions {
                concurrency: 0,
                ..ok
            },
            PrepareOptions {
                concurrency: 17,
                ..ok
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
