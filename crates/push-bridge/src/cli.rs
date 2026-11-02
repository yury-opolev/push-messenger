//! The command line: `push-agent-bridge [--dir <folder>] <command>`.

use std::ffi::OsString;
use std::path::PathBuf;

pub const USAGE: &str = "\
usage: push-agent-bridge [--dir <folder>] <command>

commands:
  keygen                 create the key if needed; print its storage and thumbprint
  thumbprint             print the thumbprint of the existing key
  enroll --code <code>   enroll with the owner's one-time code and register this device
  run                    run the bridge (what the scheduled task runs)
  status                 print the configuration path, the key, the device, the folder
                         and the sessions (no secrets)
  pause [--reason <text>] [--drain-timeout <seconds>]
                         ask the running bridge to pause Coda for an upgrade: Coda
                         finishes its work (600 seconds at most by default), saves its
                         state and is stopped; it starts again with its state once its
                         program changed (or on resume)
  resume                 ask the running bridge to start Coda again now (or to cancel a
                         pause that is still draining)
  help                   print this help

--dir names the bridge's folder (config.json, state.json, logs); by default
%LOCALAPPDATA%\\push-messenger\\coda-bridge on Windows.

exit codes: 0 done; 1 failed; 2 not set up (no key, not enrolled, or the key is not
accepted: enroll again); 3 the key provider failed.";

/// What to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Keygen,
    Thumbprint,
    Enroll {
        code: String,
    },
    Run,
    Status,
    /// Pauses the running bridge's backend for an upgrade.
    Pause {
        reason: Option<String>,
        /// How long the backend may finish its work, in seconds.
        drain_timeout: Option<u64>,
    },
    /// Starts the running bridge's paused backend again.
    Resume,
    Help,
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    /// The bridge's folder.
    pub dir: PathBuf,
    pub command: Command,
}

/// Parses the arguments after the program's name. `--dir` may come anywhere and
/// defaults to `default_dir()`; `--name value` and `--name=value` are both accepted.
pub fn parse(
    args: impl IntoIterator<Item = OsString>,
    default_dir: impl FnOnce() -> PathBuf,
) -> Result<Cli, String> {
    let mut dir: Option<PathBuf> = None;
    let mut code: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut drain_timeout: Option<u64> = None;
    let mut words: Vec<String> = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy().into_owned();
        let (name, inline) = match text.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name.to_owned(), Some(value)),
            _ => (text.clone(), None),
        };
        match name.as_str() {
            "--dir" => {
                if dir.is_some() {
                    return Err("--dir is given twice".to_owned());
                }
                let value = match inline {
                    Some(value) => OsString::from(value),
                    None => args.next().ok_or("--dir needs a folder")?,
                };
                if value.is_empty() {
                    return Err("--dir needs a folder".to_owned());
                }
                dir = Some(PathBuf::from(value));
            }
            "--code" => {
                if code.is_some() {
                    return Err("--code is given twice".to_owned());
                }
                let value = match inline {
                    Some(value) => value.to_owned(),
                    None => args
                        .next()
                        .ok_or("--code needs the enrollment code")?
                        .to_string_lossy()
                        .into_owned(),
                };
                let value = value.trim();
                if value.is_empty() {
                    return Err("--code needs the enrollment code".to_owned());
                }
                code = Some(value.to_owned());
            }
            "--reason" => {
                if reason.is_some() {
                    return Err("--reason is given twice".to_owned());
                }
                let value = match inline {
                    Some(value) => value.to_owned(),
                    None => args
                        .next()
                        .ok_or("--reason needs a text")?
                        .to_string_lossy()
                        .into_owned(),
                };
                let value = value.trim();
                if value.is_empty() {
                    return Err("--reason needs a text".to_owned());
                }
                reason = Some(value.to_owned());
            }
            "--drain-timeout" => {
                if drain_timeout.is_some() {
                    return Err("--drain-timeout is given twice".to_owned());
                }
                let value = match inline {
                    Some(value) => value.to_owned(),
                    None => args
                        .next()
                        .ok_or("--drain-timeout needs seconds")?
                        .to_string_lossy()
                        .into_owned(),
                };
                let seconds = value
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| "--drain-timeout needs a whole number of seconds")?;
                drain_timeout = Some(seconds);
            }
            "--help" | "-h" => words.push("help".to_owned()),
            _ if text.starts_with('-') => return Err(format!("unknown option {text}")),
            _ => words.push(text),
        }
    }
    let command = match words.as_slice() {
        [] => return Err("no command given".to_owned()),
        [word] => match word.as_str() {
            "keygen" => Command::Keygen,
            "thumbprint" => Command::Thumbprint,
            "enroll" => match code.take() {
                Some(code) => Command::Enroll { code },
                None => return Err("enroll needs --code <code>".to_owned()),
            },
            "run" => Command::Run,
            "status" => Command::Status,
            "pause" => Command::Pause {
                reason: reason.take(),
                drain_timeout: drain_timeout.take(),
            },
            "resume" => Command::Resume,
            "help" => Command::Help,
            other => return Err(format!("unknown command {other}")),
        },
        [_, extra, ..] => return Err(format!("unexpected argument {extra}")),
    };
    if code.is_some() {
        return Err("--code belongs to enroll".to_owned());
    }
    if reason.is_some() {
        return Err("--reason belongs to pause".to_owned());
    }
    if drain_timeout.is_some() {
        return Err("--drain-timeout belongs to pause".to_owned());
    }
    Ok(Cli {
        dir: dir.unwrap_or_else(default_dir),
        command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Result<Cli, String> {
        parse(args.iter().map(OsString::from), || {
            PathBuf::from("default-dir")
        })
    }

    fn command(args: &[&str]) -> Command {
        parsed(args).unwrap().command
    }

    #[test]
    fn commands_parse() {
        assert_eq!(command(&["keygen"]), Command::Keygen);
        assert_eq!(command(&["thumbprint"]), Command::Thumbprint);
        assert_eq!(command(&["run"]), Command::Run);
        assert_eq!(command(&["status"]), Command::Status);
        for help in ["help", "--help", "-h"] {
            assert_eq!(command(&[help]), Command::Help);
        }
        let enroll = Command::Enroll {
            code: "ABCD-1234".into(),
        };
        assert_eq!(command(&["enroll", "--code", "ABCD-1234"]), enroll);
        assert_eq!(command(&["--code=ABCD-1234", "enroll"]), enroll);
        assert_eq!(command(&["enroll", "--code", " ABCD-1234 "]), enroll);
    }

    #[test]
    fn the_folder_defaults_and_may_come_anywhere() {
        assert_eq!(parsed(&["run"]).unwrap().dir, PathBuf::from("default-dir"));
        for args in [
            &["--dir", r"C:\agents\one", "run"][..],
            &["run", "--dir", r"C:\agents\one"],
            &[r"--dir=C:\agents\one", "run"],
        ] {
            assert_eq!(
                parsed(args).unwrap(),
                Cli {
                    dir: PathBuf::from(r"C:\agents\one"),
                    command: Command::Run
                }
            );
        }
    }

    #[test]
    fn pause_and_resume_parse() {
        assert_eq!(
            command(&["pause"]),
            Command::Pause {
                reason: None,
                drain_timeout: None
            }
        );
        assert_eq!(
            command(&["pause", "--reason", "x", "--drain-timeout", "30"]),
            Command::Pause {
                reason: Some("x".into()),
                drain_timeout: Some(30)
            }
        );
        assert_eq!(
            command(&["--drain-timeout=0", "pause", "--reason=new build"]),
            Command::Pause {
                reason: Some("new build".into()),
                drain_timeout: Some(0)
            }
        );
        assert_eq!(command(&["resume"]), Command::Resume);
    }

    #[test]
    fn mistakes_are_refused_with_a_reason() {
        for (args, reason) in [
            (&[][..], "no command given"),
            (&["start"], "unknown command start"),
            (&["run", "status"], "unexpected argument status"),
            (&["--verbose", "run"], "unknown option --verbose"),
            (&["run", "--dir"], "--dir needs a folder"),
            (&["--dir=", "run"], "--dir needs a folder"),
            (&["--dir", "a", "--dir", "b", "run"], "--dir is given twice"),
            (&["enroll"], "enroll needs --code <code>"),
            (&["enroll", "--code"], "--code needs the enrollment code"),
            (
                &["enroll", "--code", "  "],
                "--code needs the enrollment code",
            ),
            (&["run", "--code", "x"], "--code belongs to enroll"),
            (&["pause", "--reason"], "--reason needs a text"),
            (&["pause", "--reason", " "], "--reason needs a text"),
            (
                &["pause", "--reason", "a", "--reason", "b"],
                "--reason is given twice",
            ),
            (
                &["pause", "--drain-timeout"],
                "--drain-timeout needs seconds",
            ),
            (
                &["pause", "--drain-timeout", "soon"],
                "--drain-timeout needs a whole number of seconds",
            ),
            (
                &["pause", "--drain-timeout", "-5"],
                "--drain-timeout needs a whole number of seconds",
            ),
            (&["resume", "--reason", "x"], "--reason belongs to pause"),
            (
                &["run", "--drain-timeout", "5"],
                "--drain-timeout belongs to pause",
            ),
        ] {
            assert_eq!(parsed(args).unwrap_err(), reason, "{args:?}");
        }
    }
}
