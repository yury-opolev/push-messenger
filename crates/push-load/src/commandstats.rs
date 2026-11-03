//! Redis `INFO commandstats` before and after a calls run: what the run cost the target's
//! Redis, command by command. Optional, as it needs the Redis URL, which holds a
//! credential: it comes from a file, is never printed, and no error repeats it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Serialize;

/// One `INFO commandstats` read takes at most this long.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A `redis://` or `rediss://` URL. Its `Debug` hides it: it may hold a password.
#[derive(Clone, PartialEq, Eq)]
pub struct RedisUrl(String);

impl fmt::Debug for RedisUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RedisUrl(..)")
    }
}

impl RedisUrl {
    pub fn new(url: &str) -> anyhow::Result<Self> {
        let url = url.trim();
        if !(url.starts_with("redis://") || url.starts_with("rediss://")) {
            bail!("the Redis URL must start with redis:// or rediss://");
        }
        if redis::Client::open(url).is_err() {
            bail!("the Redis URL is not valid");
        }
        Ok(Self(url.to_owned()))
    }

    /// The URL in `path`, the only content of the file.
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read the Redis URL file {}", path.display()))?;
        Self::new(&text).with_context(|| format!("in {}", path.display()))
    }

    /// The command statistics now; an error names its kind, never the URL.
    pub async fn read_commandstats(&self) -> Result<CommandStats, String> {
        let read = async {
            let client = redis::Client::open(self.0.as_str()).map_err(failure)?;
            let mut connection = client
                .get_multiplexed_async_connection()
                .await
                .map_err(failure)?;
            redis::cmd("INFO")
                .arg("commandstats")
                .query_async::<String>(&mut connection)
                .await
                .map_err(failure)
        };
        match tokio::time::timeout(READ_TIMEOUT, read).await {
            Ok(info) => Ok(parse(&info?)),
            Err(_) => Err(format!("no answer within {} s", READ_TIMEOUT.as_secs())),
        }
    }
}

/// A Redis error by its kind and code only: its text may echo the URL or a reply.
fn failure(error: redis::RedisError) -> String {
    match error.code() {
        Some(code) => format!("{:?} ({code})", error.kind()),
        None => format!("{:?}", error.kind()),
    }
}

/// One command's line of `INFO commandstats`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommandStat {
    pub calls: u64,
    pub usec: u64,
    pub rejected_calls: u64,
    pub failed_calls: u64,
}

/// Commands by lowercase name (`evalsha`, `client|setname`).
pub type CommandStats = BTreeMap<String, CommandStat>;

/// `INFO commandstats` as Redis 6 and 7 write it:
/// `cmdstat_evalsha:calls=10,usec=200,usec_per_call=20.00,rejected_calls=0,failed_calls=0`.
/// Lines it does not know are skipped.
pub fn parse(info: &str) -> CommandStats {
    let mut stats = CommandStats::new();
    for line in info.lines() {
        let Some((name, fields)) = line
            .trim()
            .strip_prefix("cmdstat_")
            .and_then(|rest| rest.split_once(':'))
        else {
            continue;
        };
        let mut stat = CommandStat::default();
        for field in fields.split(',') {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            let Ok(value) = value.parse::<u64>() else {
                continue;
            };
            match key {
                "calls" => stat.calls = value,
                "usec" => stat.usec = value,
                "rejected_calls" => stat.rejected_calls = value,
                "failed_calls" => stat.failed_calls = value,
                _ => {}
            }
        }
        stats.insert(name.to_ascii_lowercase(), stat);
    }
    stats
}

/// What one command cost during the run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandDelta {
    pub calls: u64,
    pub usec: u64,
    pub usec_per_call: f64,
    pub rejected_calls: u64,
    pub failed_calls: u64,
}

/// The report's `redis` section.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RedisDeltas {
    /// Commands called during the run, by name, with their deltas.
    pub commands: BTreeMap<String, CommandDelta>,
    pub total_calls: u64,
    pub total_usec: u64,
    /// Redis commands per call placed, when calls were placed.
    pub calls_per_call_placed: Option<f64>,
    /// Counters went down: the statistics were reset during the run (`CONFIG RESETSTAT`
    /// or a restart); the deltas cover only part of it.
    pub reset: bool,
    /// Why the statistics after the run could not be read.
    pub error: Option<String>,
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

impl RedisDeltas {
    /// The run's deltas; commands not called during the run are left out.
    pub fn between(before: &CommandStats, after: &CommandStats, calls_placed: u64) -> Self {
        let mut deltas = Self::default();
        for (name, now) in after {
            let was = before.get(name).copied().unwrap_or_default();
            if now.calls < was.calls || now.usec < was.usec {
                deltas.reset = true;
                continue;
            }
            let calls = now.calls - was.calls;
            if calls == 0 {
                continue;
            }
            let usec = now.usec - was.usec;
            deltas.total_calls += calls;
            deltas.total_usec += usec;
            deltas.commands.insert(
                name.clone(),
                CommandDelta {
                    calls,
                    usec,
                    usec_per_call: round2(usec as f64 / calls as f64),
                    rejected_calls: now.rejected_calls.saturating_sub(was.rejected_calls),
                    failed_calls: now.failed_calls.saturating_sub(was.failed_calls),
                },
            );
        }
        if before.keys().any(|name| !after.contains_key(name)) {
            deltas.reset = true;
        }
        deltas.calls_per_call_placed =
            (calls_placed > 0).then(|| round2(deltas.total_calls as f64 / calls_placed as f64));
        deltas
    }

    pub fn failed(error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::default()
        }
    }

    /// The most called commands first, at most `count`.
    pub fn busiest(&self, count: usize) -> Vec<(&str, &CommandDelta)> {
        let mut commands: Vec<_> = self
            .commands
            .iter()
            .map(|(name, delta)| (name.as_str(), delta))
            .collect();
        commands.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then(a.0.cmp(b.0)));
        commands.truncate(count);
        commands
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = "# Commandstats\r\n\
        cmdstat_evalsha:calls=10,usec=200,usec_per_call=20.00,rejected_calls=0,failed_calls=0\r\n\
        cmdstat_info:calls=3,usec=90,usec_per_call=30.00,rejected_calls=0,failed_calls=0\r\n\
        cmdstat_client|setname:calls=2,usec=4,usec_per_call=2.00,rejected_calls=0,failed_calls=0\r\n";
    const AFTER: &str = "# Commandstats\r\n\
        cmdstat_evalsha:calls=40,usec=1100,usec_per_call=27.50,rejected_calls=0,failed_calls=1\r\n\
        cmdstat_info:calls=4,usec=120,usec_per_call=30.00,rejected_calls=0,failed_calls=0\r\n\
        cmdstat_client|setname:calls=2,usec=4,usec_per_call=2.00,rejected_calls=0,failed_calls=0\r\n\
        cmdstat_ZSCORE:calls=5,usec=10,usec_per_call=2.00,rejected_calls=1,failed_calls=0\r\n\
        not a stat line\r\n";

    #[test]
    fn commandstats_lines_are_parsed_by_lowercase_name() {
        let stats = parse(AFTER);
        assert_eq!(stats.len(), 4);
        assert_eq!(
            stats["evalsha"],
            CommandStat {
                calls: 40,
                usec: 1100,
                rejected_calls: 0,
                failed_calls: 1
            }
        );
        assert_eq!(stats["zscore"].rejected_calls, 1);
        assert_eq!(stats["client|setname"].calls, 2);
        // Redis 6.0 writes only calls, usec and usec_per_call.
        let old = parse("cmdstat_get:calls=7,usec=14,usec_per_call=2.00");
        assert_eq!((old["get"].calls, old["get"].usec), (7, 14));
        assert!(parse("# Commandstats\r\n").is_empty());
    }

    #[test]
    fn deltas_leave_out_commands_not_called_during_the_run() {
        let deltas = RedisDeltas::between(&parse(BEFORE), &parse(AFTER), 10);
        assert_eq!(
            deltas.commands.keys().collect::<Vec<_>>(),
            ["evalsha", "info", "zscore"]
        );
        assert_eq!(
            deltas.commands["evalsha"],
            CommandDelta {
                calls: 30,
                usec: 900,
                usec_per_call: 30.0,
                rejected_calls: 0,
                failed_calls: 1
            }
        );
        assert_eq!(deltas.commands["zscore"].calls, 5, "new since the start");
        assert_eq!((deltas.total_calls, deltas.total_usec), (36, 940));
        assert_eq!(deltas.calls_per_call_placed, Some(3.6));
        assert!(!deltas.reset && deltas.error.is_none());
        assert_eq!(
            deltas
                .busiest(2)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            ["evalsha", "zscore"]
        );
        assert_eq!(
            RedisDeltas::between(&parse(BEFORE), &parse(AFTER), 0).calls_per_call_placed,
            None
        );
    }

    #[test]
    fn statistics_reset_during_the_run_are_flagged() {
        let deltas = RedisDeltas::between(&parse(AFTER), &parse(BEFORE), 1);
        assert!(deltas.reset);
        assert!(deltas.commands.is_empty());
    }

    #[test]
    fn the_url_is_checked_and_never_shown() {
        let url = RedisUrl::new(" redis://load:s3cret@redis.example.com:6380 \n").unwrap();
        assert_eq!(format!("{url:?}"), "RedisUrl(..)");
        assert_eq!(url.0, "redis://load:s3cret@redis.example.com:6380");
        for bad in ["", "http://redis.example.com", "s3cret"] {
            let error = RedisUrl::new(bad).unwrap_err().to_string();
            assert!(!error.contains("s3cret"), "{error}");
        }
    }

    #[tokio::test]
    async fn an_unreachable_redis_is_an_error_without_the_url() {
        // Port 1 on loopback: refused at once.
        let url = RedisUrl::new("redis://load:s3cret@127.0.0.1:1").unwrap();
        let error = url.read_commandstats().await.unwrap_err();
        assert!(
            !error.contains("s3cret") && !error.contains("127.0.0.1"),
            "{error}"
        );
    }
}
