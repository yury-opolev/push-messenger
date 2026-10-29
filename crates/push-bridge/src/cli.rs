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
    Enroll { code: String },
    Run,
    Status,
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
            "help" => Command::Help,
            other => return Err(format!("unknown command {other}")),
        },
        [_, extra, ..] => return Err(format!("unexpected argument {extra}")),
    };
    if code.is_some() {
        return Err("--code belongs to enroll".to_owned());
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
        ] {
            assert_eq!(parsed(args).unwrap_err(), reason, "{args:?}");
        }
    }
}
