//! The owner's commands: `/new`, `/cd <path>` and the rest. Any other `/…` is a prompt
//! for the backend.

use std::fmt;

/// One parsed command.
#[derive(Clone, PartialEq, Eq)]
pub enum Command {
    New,
    Stop,
    Status,
    Help,
    /// The folder as typed (surrounding double quotes removed); never empty.
    Cd(String),
    Compact,
    Model(Option<String>),
    /// One of [`EFFORTS`].
    Effort(String),
    /// One of [`MODES`].
    Mode(String),
    /// Empty clears the goal.
    Goal(String),
    Sessions,
    Resume(String),
    Fork,
    Tasks,
    TaskStop(String),
    Btw(String),
    ScheduleList,
    ScheduleAdd {
        when: ScheduleWhen,
        prompt: String,
    },
    ScheduleRemove(String),
    Skills,
    /// Any other `/…`: sent to the backend as a prompt.
    Passthrough(String),
    /// A known command with bad arguments: the usage line to reply with.
    Usage(&'static str),
}

/// When a scheduled prompt runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleWhen {
    /// An interval such as `1h`.
    Every(String),
    /// A time such as `2026-10-29T09:00`.
    At(String),
    /// A five-field cron spec.
    Cron(String),
}

/// The values `/effort` accepts.
pub const EFFORTS: [&str; 6] = ["low", "medium", "high", "xhigh", "max", "auto"];
/// The values `/mode` accepts; `bypass` asks for confirmation first.
pub const MODES: [&str; 4] = ["default", "acceptEdits", "plan", "bypass"];

const CD_USAGE: &str = "/cd <path>";
const MODEL_USAGE: &str = "/model [id]";
const EFFORT_USAGE: &str = "/effort <low|medium|high|xhigh|max|auto>";
const MODE_USAGE: &str = "/mode <default|acceptEdits|plan|bypass>";
const YOLO_USAGE: &str = "/yolo";
const RESUME_USAGE: &str = "/resume <id>";
const TASK_USAGE: &str = "/task stop <id>";
const BTW_USAGE: &str = "/btw <question>";
const SCHEDULE_USAGE: &str =
    "/schedule list | /schedule add <every|at|cron> <when> <prompt> | /schedule rm <id>";

/// The reply to `/help`: every command, one line each.
pub const HELP: &str = "\
/new: start a new session in this folder (the permission mode returns to the default)
/cd <path>: switch to a folder inside the allowed roots
/stop: interrupt the running turn
/status: session, folder, model, permission mode, busy or idle, running tasks, version
/compact: compact the conversation (when idle)
/model [id]: list the models, or switch to one
/effort <low|medium|high|xhigh|max|auto>: set the reasoning effort
/mode <default|acceptEdits|plan>: set the permission mode
/mode bypass: turn approvals off until /new (asks for confirmation first)
/yolo: the same as /mode bypass
/goal [text]: set the session's goal; without text, clear it
/sessions: list this folder's sessions
/resume <id>: resume one of this folder's sessions
/fork: fork the session (when idle)
/tasks: list the running tasks
/task stop <id>: stop a task
/btw <question>: ask aside; the answer stays out of the conversation
/schedule list | add <every|at|cron> <when> <prompt> | rm <id>: scheduled prompts (lost when the agent restarts)
/skills: list the skills
/help: this list
Any other /command goes to the agent as a prompt.";

/// `None` when `text` is not a command (does not start with '/').
pub fn parse(text: &str) -> Option<Command> {
    let text = text.trim();
    let line = text.strip_prefix('/')?;
    let (word, arguments) = split_word(line);
    let no_arguments = |command: Command, usage: &'static str| {
        if arguments.is_empty() {
            command
        } else {
            Command::Usage(usage)
        }
    };
    let command = match word {
        "new" => no_arguments(Command::New, "/new"),
        "stop" => no_arguments(Command::Stop, "/stop"),
        "status" => no_arguments(Command::Status, "/status"),
        "help" => no_arguments(Command::Help, "/help"),
        "compact" => no_arguments(Command::Compact, "/compact"),
        "sessions" => no_arguments(Command::Sessions, "/sessions"),
        "fork" => no_arguments(Command::Fork, "/fork"),
        "tasks" => no_arguments(Command::Tasks, "/tasks"),
        "skills" => no_arguments(Command::Skills, "/skills"),
        "cd" => {
            let path = unquote(arguments);
            if path.is_empty() {
                Command::Usage(CD_USAGE)
            } else {
                Command::Cd(path.to_owned())
            }
        }
        "model" => match one_word(arguments) {
            Some("") => Command::Model(None),
            Some(id) => Command::Model(Some(id.to_owned())),
            None => Command::Usage(MODEL_USAGE),
        },
        "effort" if EFFORTS.contains(&arguments) => Command::Effort(arguments.to_owned()),
        "effort" => Command::Usage(EFFORT_USAGE),
        "mode" if MODES.contains(&arguments) => Command::Mode(arguments.to_owned()),
        "mode" => Command::Usage(MODE_USAGE),
        // Coda's terminal name for `/mode bypass`.
        "yolo" => no_arguments(Command::Mode("bypass".to_owned()), YOLO_USAGE),
        "goal" => Command::Goal(arguments.to_owned()),
        "resume" => match one_word(arguments) {
            Some("") | None => Command::Usage(RESUME_USAGE),
            Some(id) => Command::Resume(id.to_owned()),
        },
        "task" => match split_word(arguments) {
            ("stop", id) => match one_word(id) {
                Some("") | None => Command::Usage(TASK_USAGE),
                Some(id) => Command::TaskStop(id.to_owned()),
            },
            _ => Command::Usage(TASK_USAGE),
        },
        "btw" if arguments.is_empty() => Command::Usage(BTW_USAGE),
        "btw" => Command::Btw(arguments.to_owned()),
        "schedule" => schedule(arguments).unwrap_or(Command::Usage(SCHEDULE_USAGE)),
        _ => Command::Passthrough(text.to_owned()),
    };
    Some(command)
}

fn schedule(arguments: &str) -> Option<Command> {
    match split_word(arguments) {
        ("list", "") => Some(Command::ScheduleList),
        ("rm", id) => match one_word(id) {
            Some("") | None => None,
            Some(id) => Some(Command::ScheduleRemove(id.to_owned())),
        },
        ("add", rest) => {
            let (kind, rest) = split_word(rest);
            let (spec, prompt) = match kind {
                "every" | "at" => quoted_or_word(rest)?,
                "cron" => cron_spec(rest)?,
                _ => return None,
            };
            if spec.is_empty() || prompt.is_empty() {
                return None;
            }
            let when = match kind {
                "every" => ScheduleWhen::Every(spec),
                "at" => ScheduleWhen::At(spec),
                _ => ScheduleWhen::Cron(spec),
            };
            Some(Command::ScheduleAdd {
                when,
                prompt: prompt.to_owned(),
            })
        }
        _ => None,
    }
}

/// The first word and the rest, trimmed.
fn split_word(text: &str) -> (&str, &str) {
    match text.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (text, ""),
    }
}

/// `Some("")` for no argument, `Some(word)` for one word, `None` for more.
fn one_word(text: &str) -> Option<&str> {
    match split_word(text) {
        (word, "") => Some(word),
        _ => None,
    }
}

fn unquote(text: &str) -> &str {
    text.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(text)
        .trim()
}

/// A `"quoted spec"` or one word, then the rest.
fn quoted_or_word(text: &str) -> Option<(String, &str)> {
    match text.strip_prefix('"') {
        Some(inner) => {
            let (spec, rest) = inner.split_once('"')?;
            Some((spec.trim().to_owned(), rest.trim()))
        }
        None => {
            let (word, rest) = split_word(text);
            Some((word.to_owned(), rest))
        }
    }
}

/// A `"quoted cron spec"` or five space-separated fields, then the rest.
fn cron_spec(text: &str) -> Option<(String, &str)> {
    if text.starts_with('"') {
        return quoted_or_word(text);
    }
    let mut fields = Vec::with_capacity(5);
    let mut rest = text;
    for _ in 0..5 {
        let (field, after) = split_word(rest);
        if field.is_empty() {
            return None;
        }
        fields.push(field);
        rest = after;
    }
    Some((fields.join(" "), rest))
}

impl Command {
    /// The command's name, for logs: never its arguments.
    pub fn name(&self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Stop => "stop",
            Self::Status => "status",
            Self::Help => "help",
            Self::Cd(_) => "cd",
            Self::Compact => "compact",
            Self::Model(_) => "model",
            Self::Effort(_) => "effort",
            Self::Mode(_) => "mode",
            Self::Goal(_) => "goal",
            Self::Sessions => "sessions",
            Self::Resume(_) => "resume",
            Self::Fork => "fork",
            Self::Tasks => "tasks",
            Self::TaskStop(_) => "task stop",
            Self::Btw(_) => "btw",
            Self::ScheduleList => "schedule list",
            Self::ScheduleAdd { .. } => "schedule add",
            Self::ScheduleRemove(_) => "schedule rm",
            Self::Skills => "skills",
            Self::Passthrough(_) => "passthrough",
            Self::Usage(_) => "usage",
        }
    }
}

/// The owner's words stay out of `Debug` output (and so out of logs): only the command's
/// name and its checked arguments show.
impl fmt::Debug for Command {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Effort(value) | Self::Mode(value) => {
                write!(formatter, "Command({} {value})", self.name())
            }
            Self::Usage(usage) => write!(formatter, "Command(usage {usage})"),
            _ => write!(formatter, "Command({})", self.name()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Command {
        parse(text).expect("a command")
    }

    #[test]
    fn plain_text_is_no_command() {
        assert_eq!(parse("hello /new"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn commands_without_arguments() {
        assert_eq!(parsed("/new"), Command::New);
        assert_eq!(parsed("  /stop \n"), Command::Stop);
        assert_eq!(parsed("/status"), Command::Status);
        assert_eq!(parsed("/help"), Command::Help);
        assert_eq!(parsed("/compact"), Command::Compact);
        assert_eq!(parsed("/sessions"), Command::Sessions);
        assert_eq!(parsed("/fork"), Command::Fork);
        assert_eq!(parsed("/tasks"), Command::Tasks);
        assert_eq!(parsed("/skills"), Command::Skills);
        assert_eq!(parsed("/new please"), Command::Usage("/new"));
    }

    #[test]
    fn cd_takes_the_rest_as_the_path() {
        assert_eq!(
            parsed(r"/cd C:\My Projects\app"),
            Command::Cd(r"C:\My Projects\app".into())
        );
        assert_eq!(
            parsed(r#"/cd "C:\My Projects""#),
            Command::Cd(r"C:\My Projects".into())
        );
        assert_eq!(parsed("/cd ../other"), Command::Cd("../other".into()));
        assert_eq!(parsed("/cd"), Command::Usage(CD_USAGE));
        assert_eq!(parsed("/cd \"\""), Command::Usage(CD_USAGE));
    }

    #[test]
    fn model_with_and_without_an_id() {
        assert_eq!(parsed("/model"), Command::Model(None));
        assert_eq!(parsed("/model gpt-5"), Command::Model(Some("gpt-5".into())));
        assert_eq!(parsed("/model a b"), Command::Usage(MODEL_USAGE));
    }

    #[test]
    fn effort_accepts_only_the_known_levels() {
        for effort in EFFORTS {
            assert_eq!(
                parsed(&format!("/effort {effort}")),
                Command::Effort(effort.into())
            );
        }
        assert_eq!(parsed("/effort extreme"), Command::Usage(EFFORT_USAGE));
        assert_eq!(parsed("/effort"), Command::Usage(EFFORT_USAGE));
        assert_eq!(parsed("/effort High"), Command::Usage(EFFORT_USAGE));
    }

    #[test]
    fn mode_accepts_the_known_modes() {
        assert_eq!(parsed("/mode default"), Command::Mode("default".into()));
        assert_eq!(
            parsed("/mode acceptEdits"),
            Command::Mode("acceptEdits".into())
        );
        assert_eq!(parsed("/mode plan"), Command::Mode("plan".into()));
        assert_eq!(parsed("/mode bypass"), Command::Mode("bypass".into()));
        assert_eq!(parsed("/mode yolo"), Command::Usage(MODE_USAGE));
        assert_eq!(parsed("/mode"), Command::Usage(MODE_USAGE));
    }

    #[test]
    fn yolo_is_mode_bypass_without_arguments() {
        assert_eq!(parsed("/yolo"), parsed("/mode bypass"));
        assert_eq!(parsed(" /yolo \n"), Command::Mode("bypass".into()));
        assert_eq!(parsed("/yolo now"), Command::Usage(YOLO_USAGE));
        assert_eq!(parsed("/yolo bypass"), Command::Usage(YOLO_USAGE));
        assert_eq!(parsed("/YOLO"), Command::Passthrough("/YOLO".into()));
    }

    #[test]
    fn goal_with_text_or_empty() {
        assert_eq!(
            parsed("/goal ship the bridge"),
            Command::Goal("ship the bridge".into())
        );
        assert_eq!(parsed("/goal"), Command::Goal(String::new()));
    }

    #[test]
    fn resume_needs_one_id() {
        assert_eq!(parsed("/resume abc-123"), Command::Resume("abc-123".into()));
        assert_eq!(parsed("/resume"), Command::Usage(RESUME_USAGE));
        assert_eq!(parsed("/resume a b"), Command::Usage(RESUME_USAGE));
    }

    #[test]
    fn task_stop_needs_an_id() {
        assert_eq!(parsed("/task stop t1"), Command::TaskStop("t1".into()));
        assert_eq!(parsed("/task stop"), Command::Usage(TASK_USAGE));
        assert_eq!(parsed("/task start t1"), Command::Usage(TASK_USAGE));
        assert_eq!(parsed("/task"), Command::Usage(TASK_USAGE));
    }

    #[test]
    fn btw_needs_a_question() {
        assert_eq!(parsed("/btw what is X?"), Command::Btw("what is X?".into()));
        assert_eq!(parsed("/btw"), Command::Usage(BTW_USAGE));
    }

    #[test]
    fn schedule_list_and_remove() {
        assert_eq!(parsed("/schedule list"), Command::ScheduleList);
        assert_eq!(
            parsed("/schedule rm s1"),
            Command::ScheduleRemove("s1".into())
        );
        assert_eq!(parsed("/schedule rm"), Command::Usage(SCHEDULE_USAGE));
        assert_eq!(parsed("/schedule"), Command::Usage(SCHEDULE_USAGE));
        assert_eq!(parsed("/schedule list all"), Command::Usage(SCHEDULE_USAGE));
    }

    #[test]
    fn schedule_add_every_and_at() {
        assert_eq!(
            parsed("/schedule add every 1h run tests"),
            Command::ScheduleAdd {
                when: ScheduleWhen::Every("1h".into()),
                prompt: "run tests".into()
            }
        );
        assert_eq!(
            parsed("/schedule add at 2026-10-29T09:00 check the build"),
            Command::ScheduleAdd {
                when: ScheduleWhen::At("2026-10-29T09:00".into()),
                prompt: "check the build".into()
            }
        );
        assert_eq!(
            parsed("/schedule add at \"tomorrow 9am\" stand-up"),
            Command::ScheduleAdd {
                when: ScheduleWhen::At("tomorrow 9am".into()),
                prompt: "stand-up".into()
            }
        );
        assert_eq!(
            parsed("/schedule add every 1h"),
            Command::Usage(SCHEDULE_USAGE)
        );
        assert_eq!(
            parsed("/schedule add sometimes x"),
            Command::Usage(SCHEDULE_USAGE)
        );
    }

    #[test]
    fn schedule_add_cron_quoted_or_five_fields() {
        let standup = Command::ScheduleAdd {
            when: ScheduleWhen::Cron("0 9 * * 1-5".into()),
            prompt: "standup".into(),
        };
        assert_eq!(
            parsed("/schedule add cron \"0 9 * * 1-5\" standup"),
            standup
        );
        assert_eq!(parsed("/schedule add cron 0 9 * * 1-5 standup"), standup);
        assert_eq!(
            parsed("/schedule add cron 0 9 * *"),
            Command::Usage(SCHEDULE_USAGE)
        );
        assert_eq!(
            parsed("/schedule add cron \"0 9 * * 1-5 standup"),
            Command::Usage(SCHEDULE_USAGE)
        );
    }

    #[test]
    fn other_slash_text_passes_through() {
        assert_eq!(
            parsed("/review this"),
            Command::Passthrough("/review this".into())
        );
        assert_eq!(parsed("//x"), Command::Passthrough("//x".into()));
        assert_eq!(parsed("/"), Command::Passthrough("/".into()));
        assert_eq!(parsed("/NEW"), Command::Passthrough("/NEW".into()));
    }

    #[test]
    fn help_lists_every_command() {
        for command in [
            "/new",
            "/cd",
            "/stop",
            "/status",
            "/compact",
            "/model",
            "/effort",
            "/mode",
            "/mode bypass",
            "/yolo",
            "/goal",
            "/sessions",
            "/resume",
            "/fork",
            "/tasks",
            "/task stop",
            "/btw",
            "/schedule",
            "/skills",
            "/help",
        ] {
            assert!(
                HELP.lines().any(|line| line.starts_with(command)),
                "{command}"
            );
        }
    }

    #[test]
    fn debug_output_hides_the_owners_words() {
        let shown = format!("{:?}", parsed("/btw is the password hunter2?"));
        assert_eq!(shown, "Command(btw)");
        assert_eq!(format!("{:?}", parsed("/mode plan")), "Command(mode plan)");
    }
}
