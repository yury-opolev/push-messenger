//! Starting a voice adapter: argv as a list, never through a shell; an environment
//! without the bridge's secrets; stdin and stdout piped for push-voice-adapter/1, stderr
//! to its own log or nowhere; killed with the bridge (`kill_on_drop`, and on Windows a
//! job object that also takes whatever the adapter started).

use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::config::{SpawnConfig, StderrMode};

/// Variables never passed to the adapter (exact names, any case).
pub const SECRET_VARS: &[&str] = &["CODA_SERVE_API_KEY", "AZURE_SPEECH_KEY"];
/// Prefixes of variables never passed to the adapter (any case): the bridge's and Coda's.
pub const SECRET_PREFIXES: &[&str] = &["PUSH_", "CODA_"];
/// Variables whose names contain one of these (any case) are not passed either.
pub const SECRET_WORDS: &[&str] = &["KEY", "TOKEN", "SECRET", "PASSWORD"];
/// The adapter's stderr log in the bridge's logs folder.
pub const LOG_FILE: &str = "voice-adapter.log";
/// A log larger than this at spawn is renamed to `voice-adapter.log.1`, replacing it.
pub const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// A started adapter. Dropping it kills the adapter.
pub struct Spawned {
    /// `kill_on_drop(true)`.
    pub child: Child,
    /// Bridge → adapter.
    pub stdin: ChildStdin,
    /// Adapter → bridge.
    pub stdout: ChildStdout,
    /// Keep it as long as the child: dropping it kills the adapter's process tree.
    pub job: KillGuard,
}

/// Windows: the job object the adapter runs in (closing it kills the adapter and its
/// children); elsewhere nothing.
pub struct KillGuard {
    #[cfg(windows)]
    _job: Option<super::job::Job>,
}

/// Whether `name` (any case) is in `names`.
fn named(name: &str, names: &[String]) -> bool {
    names
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

/// Whether `name` must not reach the adapter: [`SECRET_VARS`], [`SECRET_PREFIXES`],
/// [`SECRET_WORDS`] or one of `extra_secret_names` (the variables the bridge's config
/// names as key sources), all case-insensitive.
pub fn is_secret(name: &OsStr, extra_secret_names: &[String]) -> bool {
    let upper = name.to_string_lossy().to_ascii_uppercase();
    SECRET_VARS.iter().any(|secret| upper == *secret)
        || SECRET_PREFIXES
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        || SECRET_WORDS.iter().any(|word| upper.contains(word))
        || named(&upper, extra_secret_names)
}

/// The adapter's environment: `vars` without the secret ones ([`is_secret`]), except
/// those named in `allow` (`calls.adapter.spawn.env_allow`, the owner's explicit choice;
/// any case on Windows, exact elsewhere).
pub fn adapter_env(
    vars: impl Iterator<Item = (OsString, OsString)>,
    allow: &[String],
    extra_secret_names: &[String],
) -> Vec<(OsString, OsString)> {
    vars.filter(|(name, _)| allowed(name, allow) || !is_secret(name, extra_secret_names))
        .collect()
}

fn allowed(name: &OsStr, allow: &[String]) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if cfg!(windows) {
        named(name, allow)
    } else {
        allow.iter().any(|allowed| allowed == name)
    }
}

/// Starts the adapter `config` describes, with the environment [`adapter_env`] leaves
/// and stderr appended to `<log_dir>/voice-adapter.log` (`log`) or discarded. A failure
/// names only the program's file name.
pub fn spawn(
    config: &SpawnConfig,
    log_dir: &Path,
    extra_secret_names: &[String],
) -> io::Result<Spawned> {
    spawn_with_env(config, log_dir, std::env::vars_os(), extra_secret_names)
}

fn spawn_with_env(
    config: &SpawnConfig,
    log_dir: &Path,
    vars: impl Iterator<Item = (OsString, OsString)>,
    extra_secret_names: &[String],
) -> io::Result<Spawned> {
    let Some((program, args)) = config.command.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "calls.adapter.spawn.command is empty",
        ));
    };
    let stderr = match config.stderr {
        StderrMode::Log => Stdio::from(open_log(log_dir)?),
        StderrMode::Discard => Stdio::null(),
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(adapter_env(vars, &config.env_allow, extra_secret_names))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        /// `CREATE_NO_WINDOW`: the child gets no console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| spawn_error(program, &error))?;
    let job = kill_guard(&child);
    let missing = || io::Error::other("the voice adapter has no stdio pipe");
    let stdin = child.stdin.take().ok_or_else(missing)?;
    let stdout = child.stdout.take().ok_or_else(missing)?;
    Ok(Spawned {
        child,
        stdin,
        stdout,
        job,
    })
}

/// Content-free: the program's file name and the error's kind.
fn spawn_error(program: &str, error: &io::Error) -> io::Error {
    let name = Path::new(program)
        .file_name()
        .map_or_else(|| program.into(), OsStr::to_string_lossy);
    let message = if error.kind() == io::ErrorKind::NotFound {
        format!("voice adapter program {name} was not found")
    } else {
        format!(
            "voice adapter program {name} could not be started: {}",
            error.kind()
        )
    };
    io::Error::new(error.kind(), message)
}

/// `<log_dir>/voice-adapter.log` for appending (rotated first when too large); on Unix
/// created 0600.
fn open_log(log_dir: &Path) -> io::Result<std::fs::File> {
    std::fs::create_dir_all(log_dir)?;
    let path = log_path(log_dir);
    crate::logging::rotate(&path, LOG_ROTATE_BYTES)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// `<log_dir>/voice-adapter.log`.
pub fn log_path(log_dir: &Path) -> PathBuf {
    log_dir.join(LOG_FILE)
}

#[cfg(windows)]
fn kill_guard(child: &Child) -> KillGuard {
    // A child that already exited has no handle and needs no job. Without a job the
    // child itself is still killed on drop; only its own children would survive.
    let job = child
        .raw_handle()
        .and_then(|handle| match super::job::Job::kill_on_close(handle) {
            Ok(job) => Some(job),
            Err(error) => {
                tracing::warn!(
                    event = "voice_adapter_job_failed",
                    code = error.raw_os_error()
                );
                None
            }
        });
    KillGuard { _job: job }
}

#[cfg(not(windows))]
fn kill_guard(_child: &Child) -> KillGuard {
    KillGuard {}
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncReadExt;

    use super::*;
    use crate::test_dir::TestDir;

    const HELPER: &str = "voice::adapter::spawn::tests::print_args_and_env";
    /// A test-name filter that matches nothing; the helper only prints when it sees it.
    const MARKER: &str = "voice-spawn-helper-marker";

    fn vars(names: &[&str]) -> impl Iterator<Item = (OsString, OsString)> {
        names
            .iter()
            .map(|name| (OsString::from(name), OsString::from("value")))
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn names(env: &[(OsString, OsString)]) -> Vec<String> {
        env.iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn secrets_are_not_passed() {
        let env = adapter_env(
            vars(&[
                "CODA_SERVE_API_KEY",
                "AZURE_SPEECH_KEY",
                "PUSH_AGENT_KEY",
                "push_bridge_log",
                "Coda_Home",
                "MY_API_TOKEN",
                "db_password",
                "GitHub_Secret",
                "MY_SPEECH_ENV",
                "PATH",
                "SystemRoot",
                "HOME",
                "LANG",
                "TEMP",
                "USERPROFILE",
                "APPDATA",
                "LOCALAPPDATA",
                "PATHEXT",
                "ComSpec",
                "windir",
            ]),
            &[],
            &["my_speech_env".to_owned()],
        );
        assert_eq!(
            names(&env),
            [
                "PATH",
                "SystemRoot",
                "HOME",
                "LANG",
                "TEMP",
                "USERPROFILE",
                "APPDATA",
                "LOCALAPPDATA",
                "PATHEXT",
                "ComSpec",
                "windir"
            ]
        );
    }

    #[test]
    fn allowed_variables_pass_even_when_they_look_secret() {
        let env = adapter_env(
            vars(&["MY_API_TOKEN", "OTHER_TOKEN", "PATH"]),
            &["MY_API_TOKEN".to_owned()],
            &[],
        );
        assert_eq!(names(&env), ["MY_API_TOKEN", "PATH"]);
        let env = adapter_env(vars(&["my_api_token"]), &["MY_API_TOKEN".to_owned()], &[]);
        assert_eq!(names(&env).len(), usize::from(cfg!(windows)));
    }

    /// Runs in a child process (see [`helper`]): prints its arguments and variable names.
    #[test]
    #[ignore = "a helper the spawn tests run in a child process"]
    fn print_args_and_env() {
        let args: Vec<String> = std::env::args().collect();
        if !args.iter().any(|arg| arg == MARKER) {
            return;
        }
        for arg in &args[1..] {
            println!("ARG={arg}");
        }
        for (name, _) in std::env::vars_os() {
            println!("ENV={}", name.to_string_lossy());
        }
        eprintln!("helper stderr line");
    }

    /// This test binary, running only [`print_args_and_env`], with `extra` arguments.
    fn helper(extra: &str, stderr: StderrMode) -> SpawnConfig {
        let exe = std::env::current_exe().unwrap();
        let mut command = vec![exe.to_string_lossy().into_owned()];
        command.extend(
            [
                HELPER,
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
                MARKER,
                extra,
            ]
            .map(str::to_owned),
        );
        SpawnConfig {
            command,
            stderr,
            env_allow: Vec::new(),
        }
    }

    /// The helper's printed arguments and variable names.
    async fn run(mut spawned: Spawned) -> (Vec<String>, Vec<String>) {
        drop(spawned.stdin);
        let mut out = String::new();
        let read = spawned.stdout.read_to_string(&mut out);
        tokio::time::timeout(Duration::from_secs(60), read)
            .await
            .expect("the helper finishes")
            .unwrap();
        let status = spawned.child.wait().await.unwrap();
        assert!(status.success(), "{status}");
        let lines = |prefix: &str| -> Vec<String> {
            out.lines()
                .filter_map(|line| line.strip_prefix(prefix).map(str::to_owned))
                .collect()
        };
        (lines("ARG="), lines("ENV="))
    }

    #[tokio::test]
    async fn arguments_arrive_whole_and_secrets_stay_behind() {
        let dir = TestDir::new();
        let with_secrets = std::env::vars_os().chain(vars(&[
            "CODA_SERVE_API_KEY",
            "PUSH_AGENT_KEY",
            "VOICE_SPAWN_HELPER",
        ]));
        let config = helper("a b; echo pwned", StderrMode::Discard);
        let spawned = spawn_with_env(&config, dir.path(), with_secrets, &[]).unwrap();
        let (args, env) = run(spawned).await;
        assert_eq!(args.last().map(String::as_str), Some("a b; echo pwned"));
        assert!(!args.iter().any(|arg| arg == "pwned"));
        assert!(
            env.iter().any(|name| name == "VOICE_SPAWN_HELPER"),
            "{env:?}"
        );
        assert!(!env.iter().any(|name| name == "CODA_SERVE_API_KEY"));
        assert!(!env.iter().any(|name| name == "PUSH_AGENT_KEY"));
        assert!(!log_path(dir.path()).exists(), "discard writes no log");
    }

    #[tokio::test]
    async fn stderr_goes_to_the_adapters_log() {
        let dir = TestDir::new();
        let logs = dir.path().join("logs");
        let config = helper("x", StderrMode::Log);
        let spawned = spawn_with_env(&config, &logs, std::env::vars_os(), &[]).unwrap();
        run(spawned).await;
        let log = std::fs::read_to_string(log_path(&logs)).unwrap();
        assert!(log.contains("helper stderr line"), "{log}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(log_path(&logs))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_large_log_is_rotated_at_spawn() {
        let dir = TestDir::new();
        let path = log_path(dir.path());
        std::fs::write(&path, vec![b'x'; 5 * 1024 * 1024 + 1]).unwrap();
        drop(open_log(dir.path()).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        let mut rotated = path.into_os_string();
        rotated.push(".1");
        assert!(Path::new(&rotated).exists());
    }

    #[tokio::test]
    async fn a_missing_program_names_only_its_file() {
        let dir = TestDir::new();
        let program = dir.path().join("hidden").join("voice-adapter.exe");
        let config = SpawnConfig {
            command: vec![program.to_string_lossy().into_owned()],
            stderr: StderrMode::Discard,
            env_allow: Vec::new(),
        };
        let Err(error) = spawn(&config, dir.path(), &[]) else {
            panic!("spawned a missing program");
        };
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(
            error.to_string(),
            "voice adapter program voice-adapter.exe was not found"
        );
    }
}
