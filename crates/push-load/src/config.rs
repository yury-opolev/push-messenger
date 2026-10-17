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
        let build = self.target_build.trim();
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
                sample_pairs: 51,
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
