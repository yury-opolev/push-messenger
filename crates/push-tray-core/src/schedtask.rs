//! Bridges run by a Scheduled Task (the one `install.ps1` registers), and the switch-over
//! to the tray: the tray never runs a bridge that a task runs too.

use std::io;
use std::path::{Path, PathBuf};

use push_bridge_control::instance_id;
use serde_json::Value;

use crate::autostart::Autostart;
use crate::tray_config::{Instance, TrayConfig};

/// A Scheduled Task's action (a task with several actions gives one per action).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskInfo {
    pub name: String,
    pub execute: String,
    pub arguments: String,
    pub state: String,
}

/// Reads [`LIST_SCRIPT`]'s output: one object or an array of objects with `name`,
/// `execute`, `arguments` and `state`. Lenient: a missing or non-string field is empty,
/// entries without a name are skipped, anything else is no tasks.
pub fn parse_tasks(json: &str) -> Vec<TaskInfo> {
    let json = json.trim_start_matches('\u{feff}').trim();
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let items = match value {
        Value::Array(items) => items,
        object @ Value::Object(_) => vec![object],
        _ => return Vec::new(),
    };
    items
        .iter()
        .filter_map(|item| {
            let field = |name: &str| {
                item.get(name)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            };
            let name = field("name");
            if name.is_empty() {
                return None;
            }
            Some(TaskInfo {
                name,
                execute: field("execute"),
                arguments: field("arguments"),
                state: field("state"),
            })
        })
        .collect()
}

/// The bridge folder a task runs: its action runs `push-agent-bridge(.exe)` directly or
/// through `conhost.exe --headless "<exe>"`, with `--dir "<d>"`, `--dir=<d>` or no
/// `--dir` (the default folder), and the command `run`. `None` for any other task.
pub fn bridge_dir(task: &TaskInfo, default_dir: &Path) -> Option<PathBuf> {
    let mut args = split_command_line(&task.arguments).into_iter();
    let program = program_name(&task.execute);
    if program == "conhost" {
        let exe = loop {
            let arg = args.next()?;
            match arg.as_str() {
                "--width" | "--height" | "--server" | "--signal" | "--feature" | "--vtmode" => {
                    args.next()?;
                }
                _ if arg.starts_with('-') => {}
                _ => break arg,
            }
        };
        if program_name(&exe) != "push-agent-bridge" {
            return None;
        }
    } else if program != "push-agent-bridge" {
        return None;
    }
    let mut dir = None;
    let mut words = Vec::new();
    while let Some(arg) = args.next() {
        let value = if arg == "--dir" {
            Some(args.next()?)
        } else {
            arg.strip_prefix("--dir=").map(str::to_owned)
        };
        match value {
            Some(_) if dir.is_some() => return None,
            Some(value) if value.is_empty() => return None,
            Some(value) => dir = Some(PathBuf::from(value)),
            None if arg.starts_with('-') => return None,
            None => words.push(arg),
        }
    }
    if words != ["run"] {
        return None;
    }
    Some(dir.unwrap_or_else(|| default_dir.to_path_buf()))
}

/// A program's file name without folder, quotes and `.exe`, in lowercase.
fn program_name(path: &str) -> String {
    let path = path.trim().trim_matches('"');
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    let name = name.to_lowercase();
    match name.strip_suffix(".exe") {
        Some(stem) => stem.to_owned(),
        None => name,
    }
}

/// Splits a Windows command line the way the C runtime does: whitespace separates
/// arguments outside double quotes; `2n` backslashes before a quote are `n` backslashes and
/// the quote toggles quoting, `2n+1` are `n` backslashes and a literal quote; other
/// backslashes are literal.
fn split_command_line(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut quoted = false;
    let mut backslashes = 0usize;
    for ch in line.chars() {
        match ch {
            '\\' => {
                backslashes += 1;
                in_arg = true;
            }
            '"' => {
                current.extend(std::iter::repeat_n('\\', backslashes / 2));
                if backslashes % 2 == 1 {
                    current.push('"');
                } else {
                    quoted = !quoted;
                }
                backslashes = 0;
                in_arg = true;
            }
            ' ' | '\t' if !quoted => {
                current.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            _ => {
                current.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                current.push(ch);
                in_arg = true;
            }
        }
    }
    current.extend(std::iter::repeat_n('\\', backslashes));
    if in_arg {
        args.push(current);
    }
    args
}

/// A configured bridge folder that a Scheduled Task runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFor {
    pub task: String,
    /// The folder as configured in `tray.json`.
    pub dir: PathBuf,
}

/// The tasks that run one of `dirs` (matched by instance id), in task order, without
/// repeats.
pub fn tasks_for_instances(
    tasks: &[TaskInfo],
    dirs: &[PathBuf],
    default_dir: &Path,
) -> Vec<TaskFor> {
    let ids: Vec<String> = dirs.iter().map(|dir| instance_id(dir)).collect();
    let mut found: Vec<TaskFor> = Vec::new();
    for task in tasks {
        let Some(run_dir) = bridge_dir(task, default_dir) else {
            continue;
        };
        let id = instance_id(&run_dir);
        for (dir, _) in dirs.iter().zip(&ids).filter(|(_, dir_id)| **dir_id == id) {
            let item = TaskFor {
                task: task.name.clone(),
                dir: dir.clone(),
            };
            if !found.contains(&item) {
                found.push(item);
            }
        }
    }
    found
}

/// Whether to offer the switch-over from Scheduled Tasks to the tray.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    None,
    SwitchOver(Vec<TaskFor>),
}

/// Offers the switch-over when a task runs any configured instance: the tray does not start
/// those itself (never both).
pub fn offer(found: &[TaskFor]) -> Offer {
    if found.is_empty() {
        Offer::None
    } else {
        Offer::SwitchOver(found.to_vec())
    }
}

/// The switch-over's steps, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchPlan {
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    StopTask(String),
    UnregisterTask(String),
    EnableTrayAutostart,
    MarkAutostart(PathBuf),
    StartBridge(PathBuf),
}

/// Stop and unregister each task; then turn the tray's start at logon on, and mark each
/// instance autostart and start it under the tray.
pub fn plan_switch(found: &[TaskFor]) -> SwitchPlan {
    let mut steps = Vec::new();
    let mut tasks: Vec<&str> = Vec::new();
    for item in found {
        if !tasks.contains(&item.task.as_str()) {
            tasks.push(&item.task);
            steps.push(Step::StopTask(item.task.clone()));
            steps.push(Step::UnregisterTask(item.task.clone()));
        }
    }
    if found.is_empty() {
        return SwitchPlan { steps };
    }
    steps.push(Step::EnableTrayAutostart);
    let mut ids: Vec<String> = Vec::new();
    for item in found {
        let id = instance_id(&item.dir);
        if !ids.contains(&id) {
            ids.push(id);
            steps.push(Step::MarkAutostart(item.dir.clone()));
            steps.push(Step::StartBridge(item.dir.clone()));
        }
    }
    SwitchPlan { steps }
}

/// Runs `plan` in order and stops at the first failure: a task that could not be stopped
/// or removed leaves the tray's autostart off and the instances unmarked (never both).
/// The caller saves `config`.
pub fn execute_switch(
    plan: &SwitchPlan,
    scheduler: &dyn Scheduler,
    autostart: &dyn Autostart,
    config: &mut TrayConfig,
    start: &mut dyn FnMut(&Path),
) -> io::Result<()> {
    for step in &plan.steps {
        match step {
            Step::StopTask(name) => {
                tracing::info!(event = "switch_over", step = "stop_task", task = %name);
                scheduler.stop(name)?;
            }
            Step::UnregisterTask(name) => {
                tracing::info!(event = "switch_over", step = "unregister_task", task = %name);
                scheduler.unregister(name)?;
            }
            Step::EnableTrayAutostart => autostart.set(true)?,
            Step::MarkAutostart(dir) => {
                if !config.set_autostart(dir, true) {
                    config.instances.push(Instance {
                        dir: dir.clone(),
                        autostart: true,
                    });
                }
            }
            Step::StartBridge(dir) => start(dir),
        }
    }
    Ok(())
}

/// The Scheduled Tasks of the current user.
pub trait Scheduler {
    fn list(&self) -> io::Result<Vec<TaskInfo>>;
    fn stop(&self, name: &str) -> io::Result<()>;
    fn unregister(&self, name: &str) -> io::Result<()>;
    fn run(&self, name: &str) -> io::Result<()>;
}

/// Lists every task's actions as JSON, flattened to `{name, execute, arguments, state}`.
pub const LIST_SCRIPT: &str = "Get-ScheduledTask | ForEach-Object { foreach ($a in $_.Actions) { [pscustomobject]@{ name = $_.TaskName; execute = $a.Execute; arguments = $a.Arguments; state = [string]$_.State } } } | ConvertTo-Json -Compress";

/// Runs the ScheduledTasks cmdlets through `powershell.exe` (no window, no profile),
/// passing the script as `-EncodedCommand`. Windows only: elsewhere every call fails with
/// [`io::ErrorKind::Unsupported`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PowerShellScheduler;

impl Scheduler for PowerShellScheduler {
    fn list(&self) -> io::Result<Vec<TaskInfo>> {
        Ok(parse_tasks(&run_powershell(LIST_SCRIPT)?))
    }

    fn stop(&self, name: &str) -> io::Result<()> {
        run_powershell(&task_script("Stop-ScheduledTask", name)).map(drop)
    }

    fn unregister(&self, name: &str) -> io::Result<()> {
        run_powershell(&task_script("Unregister-ScheduledTask", name)).map(drop)
    }

    fn run(&self, name: &str) -> io::Result<()> {
        run_powershell(&task_script("Start-ScheduledTask", name)).map(drop)
    }
}

/// `<cmdlet> -TaskName '<name>'` (and `-Confirm:$false` to unregister).
fn task_script(cmdlet: &str, name: &str) -> String {
    let confirm = if cmdlet == "Unregister-ScheduledTask" {
        " -Confirm:$false"
    } else {
        ""
    };
    format!("{cmdlet} -TaskName {}{confirm}", ps_quote(name))
}

/// A PowerShell single-quoted string: every quote character (PowerShell also takes the
/// typographic ones) doubled.
fn ps_quote(text: &str) -> String {
    let mut quoted = String::from("'");
    for ch in text.chars() {
        if matches!(ch, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
            quoted.push(ch);
        }
        quoted.push(ch);
    }
    quoted.push('\'');
    quoted
}

/// The full script: stop on errors, no progress output, UTF-8 output.
fn full_script(script: &str) -> String {
    format!(
        "$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'; [Console]::OutputEncoding = [System.Text.Encoding]::UTF8; {script}"
    )
}

/// `-EncodedCommand`'s argument: the script in UTF-16LE, base64.
fn encode_command(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64(&bytes)
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(triple[0]) << 16) | (u32::from(triple[1]) << 8) | u32::from(triple[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                text.push(char::from(
                    ALPHABET[((n >> (18 - 6 * index)) & 0x3f) as usize],
                ));
            } else {
                text.push('=');
            }
        }
    }
    text
}

#[cfg(windows)]
fn run_powershell(script: &str) -> io::Result<String> {
    use std::process::Stdio;
    let exe = crate::system_program(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let output = crate::hidden_command(exe)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
        ])
        .arg(encode_command(&full_script(script)))
        .stdin(Stdio::null())
        .output()?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let first = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let first: String = first.chars().take(200).collect();
    Err(io::Error::other(format!(
        "powershell.exe failed ({}): {first}",
        output.status
    )))
}

#[cfg(not(windows))]
fn run_powershell(script: &str) -> io::Result<String> {
    let _ = encode_command(&full_script(script));
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Scheduled Tasks exist only on Windows",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TestDir;
    use std::cell::RefCell;

    fn task(name: &str, execute: &str, arguments: &str) -> TaskInfo {
        TaskInfo {
            name: name.into(),
            execute: execute.into(),
            arguments: arguments.into(),
            state: "Ready".into(),
        }
    }

    fn default_dir() -> PathBuf {
        PathBuf::from("default-coda-bridge")
    }

    #[test]
    fn parse_reads_an_object_an_array_and_ignores_garbage() {
        let one = r#"{"name":"push-agent-bridge","execute":"C:\\Windows\\System32\\conhost.exe","arguments":"--headless \"C:\\b.exe\" run","state":"Running"}"#;
        assert_eq!(
            parse_tasks(one),
            [TaskInfo {
                name: "push-agent-bridge".into(),
                execute: r"C:\Windows\System32\conhost.exe".into(),
                arguments: r#"--headless "C:\b.exe" run"#.into(),
                state: "Running".into(),
            }]
        );
        let many = "\u{feff}[{\"name\":\"a\",\"execute\":\"x.exe\",\"arguments\":null,\"state\":\"Ready\"},{\"name\":\"b\"},{\"execute\":\"nameless\"},7]\r\n";
        let tasks = parse_tasks(many);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0], task("a", "x.exe", ""));
        assert_eq!(tasks[1].name, "b");
        assert_eq!(tasks[1].execute, "");
        for garbage in ["", "   ", "nope", "42", "\"text\"", "{", "null"] {
            assert!(parse_tasks(garbage).is_empty(), "{garbage:?}");
        }
    }

    #[test]
    fn the_install_ps1_conhost_form_runs_its_dir() {
        let t = task(
            "push-agent-bridge",
            r"C:\WINDOWS\System32\conhost.exe",
            r#"--headless "C:\Tools\push-agent-bridge.exe" --dir "C:\x\coda-bridge" run"#,
        );
        assert_eq!(
            bridge_dir(&t, &default_dir()),
            Some(PathBuf::from(r"C:\x\coda-bridge"))
        );
        let spaced = task(
            "t",
            r"%SystemRoot%\System32\conhost.exe",
            r#"--headless "C:\Program Files\push\push-agent-bridge.exe" --dir "C:\My Bridges\coda" run"#,
        );
        assert_eq!(
            bridge_dir(&spaced, &default_dir()),
            Some(PathBuf::from(r"C:\My Bridges\coda"))
        );
    }

    #[test]
    fn the_direct_form_the_dir_equals_form_and_the_default() {
        let direct = task(
            "t",
            r"C:\Tools\push-agent-bridge.exe",
            r#"--dir "C:\x\coda-bridge" run"#,
        );
        assert_eq!(
            bridge_dir(&direct, &default_dir()),
            Some(PathBuf::from(r"C:\x\coda-bridge"))
        );
        let quoted_exe = task(
            "t",
            r#""C:\Tools\PUSH-AGENT-BRIDGE.EXE""#,
            "run --dir=D:\\b",
        );
        assert_eq!(
            bridge_dir(&quoted_exe, &default_dir()),
            Some(PathBuf::from(r"D:\b"))
        );
        let linux = task("t", "/usr/local/bin/push-agent-bridge", "--dir=/srv/b run");
        assert_eq!(
            bridge_dir(&linux, &default_dir()),
            Some(PathBuf::from("/srv/b"))
        );
        let no_dir = task("t", r"C:\Tools\push-agent-bridge.exe", "run");
        assert_eq!(bridge_dir(&no_dir, &default_dir()), Some(default_dir()));
        let conhost_no_dir = task(
            "t",
            "conhost.exe",
            r#"--headless --width 80 "C:\Tools\push-agent-bridge.exe" run"#,
        );
        assert_eq!(
            bridge_dir(&conhost_no_dir, &default_dir()),
            Some(default_dir())
        );
    }

    #[test]
    fn other_tasks_and_other_commands_are_not_bridge_runs() {
        let cases = [
            task("t", r"C:\Windows\System32\notepad.exe", "run"),
            task(
                "t",
                r"C:\Windows\System32\conhost.exe",
                r#"--headless "C:\Tools\other.exe" --dir "C:\x" run"#,
            ),
            task(
                "t",
                r"C:\Tools\push-agent-bridge.exe",
                r#"--dir "C:\x" status"#,
            ),
            task("t", r"C:\Tools\push-agent-bridge.exe", ""),
            task("t", r"C:\Tools\push-agent-bridge.exe", "--dir"),
            task("t", r"C:\Tools\push-agent-bridge.exe", "--dir= run"),
            task(
                "t",
                r"C:\Tools\push-agent-bridge.exe",
                "--dir a --dir b run",
            ),
            task("t", r"C:\Tools\push-agent-bridge.exe", "run --verbose"),
            task("t", r"C:\Tools\push-agent-bridge.exe", "run extra"),
            task("t", r"C:\Windows\System32\conhost.exe", "--headless"),
        ];
        for t in cases {
            assert_eq!(bridge_dir(&t, &default_dir()), None, "{t:?}");
        }
    }

    #[test]
    fn command_lines_split_like_the_c_runtime() {
        assert_eq!(
            split_command_line(r#"  a "b c"  d\e "f\"g" "h\\" i"#),
            ["a", "b c", r"d\e", r#"f"g"#, r"h\", "i"]
        );
        assert_eq!(split_command_line(r#""""#), [""]);
        assert!(split_command_line("   ").is_empty());
    }

    #[test]
    fn tasks_match_configured_instances_by_id() {
        let temp = TestDir::new();
        let a = temp.bridge("a");
        let b = temp.bridge("b");
        let default = temp.bridge("default");
        let mut a_trailing = a.clone().into_os_string();
        a_trailing.push(std::path::MAIN_SEPARATOR_STR);
        let tasks = [
            task(
                "task-a",
                "push-agent-bridge",
                &format!("--dir \"{}\" run", a_trailing.to_string_lossy()),
            ),
            task(
                "task-a",
                "push-agent-bridge",
                &format!("--dir \"{}\" run", a.display()),
            ),
            task("task-default", "push-agent-bridge", "run"),
            task(
                "task-other",
                "push-agent-bridge",
                &format!("--dir \"{}\" run", temp.path().join("other").display()),
            ),
            task("notepad", "notepad.exe", ""),
        ];
        let dirs = [a.clone(), b.clone(), default.clone()];
        assert_eq!(
            tasks_for_instances(&tasks, &dirs, &default),
            [
                TaskFor {
                    task: "task-a".into(),
                    dir: a.clone(),
                },
                TaskFor {
                    task: "task-default".into(),
                    dir: default.clone(),
                },
            ]
        );
        assert!(tasks_for_instances(&tasks, &[b], &default).is_empty());
    }

    #[test]
    fn the_switch_over_is_offered_only_for_found_tasks() {
        assert_eq!(offer(&[]), Offer::None);
        let found = vec![TaskFor {
            task: "t".into(),
            dir: PathBuf::from("d"),
        }];
        assert_eq!(offer(&found), Offer::SwitchOver(found.clone()));
    }

    fn found_two() -> Vec<TaskFor> {
        vec![
            TaskFor {
                task: "t1".into(),
                dir: PathBuf::from("d1"),
            },
            TaskFor {
                task: "t2".into(),
                dir: PathBuf::from("d2"),
            },
            TaskFor {
                task: "t2".into(),
                dir: PathBuf::from("d2"),
            },
        ]
    }

    #[test]
    fn the_plan_removes_tasks_before_the_tray_takes_over() {
        assert!(plan_switch(&[]).steps.is_empty());
        assert_eq!(
            plan_switch(&found_two()).steps,
            [
                Step::StopTask("t1".into()),
                Step::UnregisterTask("t1".into()),
                Step::StopTask("t2".into()),
                Step::UnregisterTask("t2".into()),
                Step::EnableTrayAutostart,
                Step::MarkAutostart(PathBuf::from("d1")),
                Step::StartBridge(PathBuf::from("d1")),
                Step::MarkAutostart(PathBuf::from("d2")),
                Step::StartBridge(PathBuf::from("d2")),
            ]
        );
    }

    #[derive(Default)]
    struct FakeScheduler {
        calls: RefCell<Vec<String>>,
        fail_unregister: Option<String>,
    }

    impl Scheduler for FakeScheduler {
        fn list(&self) -> io::Result<Vec<TaskInfo>> {
            Ok(Vec::new())
        }
        fn stop(&self, name: &str) -> io::Result<()> {
            self.calls.borrow_mut().push(format!("stop {name}"));
            Ok(())
        }
        fn unregister(&self, name: &str) -> io::Result<()> {
            self.calls.borrow_mut().push(format!("unregister {name}"));
            if self.fail_unregister.as_deref() == Some(name) {
                return Err(io::Error::other("access denied"));
            }
            Ok(())
        }
        fn run(&self, name: &str) -> io::Result<()> {
            self.calls.borrow_mut().push(format!("run {name}"));
            Ok(())
        }
    }

    struct FakeAutostart<'a> {
        calls: &'a RefCell<Vec<String>>,
        on: RefCell<bool>,
    }

    impl Autostart for FakeAutostart<'_> {
        fn enabled(&self) -> io::Result<bool> {
            Ok(*self.on.borrow())
        }
        fn set(&self, on: bool) -> io::Result<()> {
            self.calls.borrow_mut().push(format!("autostart {on}"));
            *self.on.borrow_mut() = on;
            Ok(())
        }
    }

    fn config_with(dirs: &[&str]) -> TrayConfig {
        TrayConfig {
            instances: dirs
                .iter()
                .map(|dir| Instance {
                    dir: PathBuf::from(dir),
                    autostart: false,
                })
                .collect(),
            ..TrayConfig::default()
        }
    }

    #[test]
    fn the_switch_runs_its_steps_in_order() {
        let scheduler = FakeScheduler::default();
        let autostart = FakeAutostart {
            calls: &scheduler.calls,
            on: RefCell::new(false),
        };
        let mut config = config_with(&["d1", "d2"]);
        let mut started = Vec::new();
        execute_switch(
            &plan_switch(&found_two()),
            &scheduler,
            &autostart,
            &mut config,
            &mut |dir: &Path| {
                scheduler
                    .calls
                    .borrow_mut()
                    .push(format!("start {}", dir.display()));
                started.push(dir.to_path_buf());
            },
        )
        .unwrap();
        assert_eq!(
            *scheduler.calls.borrow(),
            [
                "stop t1",
                "unregister t1",
                "stop t2",
                "unregister t2",
                "autostart true",
                "start d1",
                "start d2",
            ]
        );
        assert!(config.instances.iter().all(|instance| instance.autostart));
        assert_eq!(started, [PathBuf::from("d1"), PathBuf::from("d2")]);
        assert!(autostart.enabled().unwrap());
    }

    #[test]
    fn a_failing_unregister_leaves_autostart_off_and_instances_unmarked() {
        let scheduler = FakeScheduler {
            fail_unregister: Some("t2".into()),
            ..FakeScheduler::default()
        };
        let autostart = FakeAutostart {
            calls: &scheduler.calls,
            on: RefCell::new(false),
        };
        let mut config = config_with(&["d1", "d2"]);
        let mut started = 0;
        let result = execute_switch(
            &plan_switch(&found_two()),
            &scheduler,
            &autostart,
            &mut config,
            &mut |_: &Path| started += 1,
        );
        assert!(result.is_err());
        assert_eq!(
            *scheduler.calls.borrow(),
            ["stop t1", "unregister t1", "stop t2", "unregister t2"]
        );
        assert!(!autostart.enabled().unwrap());
        assert!(config.instances.iter().all(|instance| !instance.autostart));
        assert_eq!(started, 0);
    }

    #[test]
    fn task_names_are_single_quoted_with_quotes_doubled() {
        assert_eq!(ps_quote("push-agent-bridge"), "'push-agent-bridge'");
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(ps_quote("a\u{2019}b"), "'a\u{2019}\u{2019}b'");
        assert_eq!(
            task_script("Stop-ScheduledTask", "x'; Remove-Item C:\\"),
            "Stop-ScheduledTask -TaskName 'x''; Remove-Item C:\\'"
        );
        assert_eq!(
            task_script("Unregister-ScheduledTask", "t"),
            "Unregister-ScheduledTask -TaskName 't' -Confirm:$false"
        );
        assert_eq!(
            task_script("Start-ScheduledTask", "t"),
            "Start-ScheduledTask -TaskName 't'"
        );
    }

    #[test]
    fn scripts_are_encoded_as_utf16le_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode_command("a"), "YQA=");
        assert_eq!(encode_command("dir"), "ZABpAHIA");
        assert!(full_script(LIST_SCRIPT).ends_with(LIST_SCRIPT));
    }
}
