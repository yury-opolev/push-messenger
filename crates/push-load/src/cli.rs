//! The command line. Credentials come only from files, never from arguments.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, bail};

use crate::config::{PrepareOptions, RunOptions, Target};
use crate::report::Status;
use crate::{accounts, prepare, report, run};

pub const USAGE: &str = "\
usage:
  push-load prepare --target <url> --authorized-target <host[:port]> --accounts <file.jsonl>
                    --state <file.json> --devices-per-account <1-10>
                    [--sample-pairs <0-50>] [--concurrency <1-16>] [--connect-to <ip>]
  push-load run     --target <url> --authorized-target <host[:port]> --state <file.json>
                    --connections <n> --ramp-per-second <n> --duration-seconds <n>
                    --target-build <label> --report <new-file.json>
                    [--message-interval-seconds <n>] [--connect-to <ip>] [--no-reconnect]

Only test a deployment you own or are authorized to test (docs/consumer-verification.md).";

struct Flags {
    values: HashMap<String, String>,
    switches: Vec<String>,
}

fn parse(args: &[String], values: &[&str], switches: &[&str]) -> anyhow::Result<Flags> {
    let mut flags = Flags {
        values: HashMap::new(),
        switches: Vec::new(),
    };
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if switches.contains(&flag.as_str()) {
            flags.switches.push(flag.clone());
            continue;
        }
        if !values.contains(&flag.as_str()) {
            bail!("unknown argument {flag:?}\n{USAGE}");
        }
        let value = args
            .next()
            .with_context(|| format!("{flag} needs a value"))?;
        if flags.values.insert(flag.clone(), value.clone()).is_some() {
            bail!("{flag} is given twice");
        }
    }
    Ok(flags)
}

impl Flags {
    fn required(&self, flag: &str) -> anyhow::Result<&str> {
        self.values
            .get(flag)
            .map(String::as_str)
            .with_context(|| format!("{flag} is required\n{USAGE}"))
    }

    fn optional(&self, flag: &str) -> Option<&str> {
        self.values.get(flag).map(String::as_str)
    }

    fn switch(&self, flag: &str) -> bool {
        self.switches.iter().any(|given| given == flag)
    }

    fn number<T: FromStr>(&self, flag: &str, default: Option<T>) -> anyhow::Result<T> {
        match (self.optional(flag), default) {
            (Some(value), _) => value
                .parse()
                .map_err(|_| anyhow::anyhow!("{flag} must be a number")),
            (None, Some(default)) => Ok(default),
            (None, None) => bail!("{flag} is required\n{USAGE}"),
        }
    }

    fn target(&self) -> anyhow::Result<Target> {
        Target::new(
            self.required("--target")?,
            self.required("--authorized-target")?,
            self.optional("--connect-to"),
        )
    }
}

/// Runs `push-load <args>`: exit 0 when the preparation is complete or the run passed,
/// 1 when not, 130 when interrupted, 2 for usage.
pub async fn main(args: Vec<String>) -> anyhow::Result<ExitCode> {
    match args.first().map(String::as_str) {
        Some("prepare") => prepare_command(&args[1..]).await,
        Some("run") => run_command(&args[1..]).await,
        _ => {
            eprintln!("{USAGE}");
            Ok(ExitCode::from(2))
        }
    }
}

async fn prepare_command(args: &[String]) -> anyhow::Result<ExitCode> {
    let flags = parse(
        args,
        &[
            "--target",
            "--authorized-target",
            "--accounts",
            "--state",
            "--devices-per-account",
            "--sample-pairs",
            "--concurrency",
            "--connect-to",
        ],
        &[],
    )?;
    let target = flags.target()?;
    let options = PrepareOptions {
        devices_per_account: flags.number("--devices-per-account", None)?,
        sample_pairs: flags.number("--sample-pairs", Some(0))?,
        concurrency: flags.number("--concurrency", Some(4))?,
    };
    options.validate()?;
    let accounts = accounts::read(&PathBuf::from(flags.required("--accounts")?))?;
    let summary = prepare::prepare(
        &target,
        &accounts,
        &PathBuf::from(flags.required("--state")?),
        &options,
    )
    .await?;
    println!(
        "push-load: prepared {} of {} devices and {} of {} sample pairs for {} accounts at {}",
        summary.devices_ready,
        summary.devices_wanted,
        summary.pairs_ready,
        summary.pairs_wanted,
        summary.accounts,
        target.base_url()
    );
    for failure in &summary.failures {
        println!("  failure: {failure}");
    }
    Ok(if summary.complete() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

async fn run_command(args: &[String]) -> anyhow::Result<ExitCode> {
    let flags = parse(
        args,
        &[
            "--target",
            "--authorized-target",
            "--state",
            "--connections",
            "--ramp-per-second",
            "--duration-seconds",
            "--message-interval-seconds",
            "--target-build",
            "--report",
            "--connect-to",
        ],
        &["--no-reconnect"],
    )?;
    let target = flags.target()?;
    let options = RunOptions {
        connections: flags.number("--connections", None)?,
        ramp_per_second: flags.number("--ramp-per-second", None)?,
        duration: Duration::from_secs(flags.number("--duration-seconds", None)?),
        message_interval: Duration::from_secs(
            flags.number("--message-interval-seconds", Some(10))?,
        ),
        target_build: flags.required("--target-build")?.to_owned(),
        reconnect: !flags.switch("--no-reconnect"),
    };
    options.validate()?;
    let report_path = PathBuf::from(flags.required("--report")?);
    if report_path.exists() {
        bail!("{} exists: name a new report file", report_path.display());
    }
    let report = run::run(
        &target,
        &PathBuf::from(flags.required("--state")?),
        &options,
    )
    .await?;
    let json = serde_json::to_string_pretty(&report).context("serialize the report")?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&report_path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, json.as_bytes()))
        .with_context(|| format!("write the report {}", report_path.display()))?;
    print!("{}", report::summary_text(&report));
    println!("  report: {}", report_path.display());
    Ok(match report.status {
        Status::Passed => ExitCode::SUCCESS,
        Status::Failed => ExitCode::from(1),
        Status::Interrupted => ExitCode::from(130),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn flags_are_explicit_and_never_repeated() {
        let flags = parse(
            &args(&["--state", "s.json", "--no-reconnect"]),
            &["--state"],
            &["--no-reconnect"],
        )
        .unwrap();
        assert_eq!(flags.required("--state").unwrap(), "s.json");
        assert!(flags.switch("--no-reconnect"));
        assert!(parse(&args(&["--unknown", "x"]), &["--state"], &[]).is_err());
        assert!(parse(&args(&["--state"]), &["--state"], &[]).is_err());
        assert!(parse(&args(&["--state", "a", "--state", "b"]), &["--state"], &[]).is_err());
        assert!(flags.required("--target").is_err());
        assert_eq!(flags.number::<u32>("--count", Some(3)).unwrap(), 3);
        assert!(flags.number::<u32>("--count", None).is_err());
    }

    #[tokio::test]
    async fn a_mismatched_authorization_stops_before_any_request() {
        let error = main(args(&[
            "run",
            "--target",
            "https://api.example.com",
            "--authorized-target",
            "other.example.com",
            "--state",
            "unused.json",
            "--connections",
            "1",
            "--ramp-per-second",
            "1",
            "--duration-seconds",
            "10",
            "--target-build",
            "x",
            "--report",
            "unused-report.json",
        ]))
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("--authorized-target"), "{error}");
    }
}
