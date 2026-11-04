//! The tray menu as data: the shell turns it into a native menu, and menu ids back into
//! commands with [`parse`]. The menu is slim: a line per agent (which opens it in the tray
//! window), then the tray's own items. The agents' actions live in the window; whether
//! one is available is [`enabled`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use push_bridge_control::CallPhase;

use crate::schedtask::Offer;
use crate::state::{
    AgentState, IconState, call_short, icon_for_view, label, tooltip as state_tooltip, worst,
};
use crate::supervisor::InstanceView;

/// An agent's item id is this followed by the instance id.
pub const AGENT_PREFIX: &str = "agent:";
/// "Open…": the tray window.
pub const OPEN: &str = "open";
/// "Add agent folder…".
pub const ADD_FOLDER: &str = "add_folder";
/// The "Start at logon" check item.
pub const START_AT_LOGON: &str = "start_at_logon";
/// "Switch from Scheduled Task…".
pub const SWITCH_OVER: &str = "switch_over";
pub const QUIT: &str = "quit";

/// An action on one agent (a button in the tray window).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentAction {
    Start,
    Stop,
    RestartBackend,
    Reconnect,
    Pause,
    Resume,
    /// `mute`: mutes the agent's voice in the running call.
    Mute,
    /// `mute` with `muted: false`.
    Unmute,
    /// `hangup`: hangs up, cancels a call ringing the owner, or declines a call waiting
    /// for the owner's confirmation.
    Hangup,
}

impl AgentAction {
    /// Every action: the agent's ([`AgentAction::AGENT`]), then the call's.
    pub const ALL: [AgentAction; 9] = [
        AgentAction::Start,
        AgentAction::Stop,
        AgentAction::RestartBackend,
        AgentAction::Reconnect,
        AgentAction::Pause,
        AgentAction::Resume,
        AgentAction::Mute,
        AgentAction::Unmute,
        AgentAction::Hangup,
    ];

    /// The agent's actions, in the window's order (the call's have their own row).
    pub const AGENT: [AgentAction; 6] = [
        AgentAction::Start,
        AgentAction::Stop,
        AgentAction::RestartBackend,
        AgentAction::Reconnect,
        AgentAction::Pause,
        AgentAction::Resume,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AgentAction::Start => "start",
            AgentAction::Stop => "stop",
            AgentAction::RestartBackend => "restart_backend",
            AgentAction::Reconnect => "reconnect",
            AgentAction::Pause => "pause",
            AgentAction::Resume => "resume",
            AgentAction::Mute => "mute",
            AgentAction::Unmute => "unmute",
            AgentAction::Hangup => "hangup",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentAction::Start => "Start",
            AgentAction::Stop => "Stop",
            AgentAction::RestartBackend => "Restart Coda",
            AgentAction::Reconnect => "Reconnect",
            AgentAction::Pause => "Pause for upgrade",
            AgentAction::Resume => "Resume",
            AgentAction::Mute => "Mute agent",
            AgentAction::Unmute => "Unmute agent",
            AgentAction::Hangup => "Hang up",
        }
    }

    /// The action [`AgentAction::as_str`] names.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.as_str() == name)
    }
}

/// What a menu item asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuCommand {
    /// Open the tray window on the agent with this instance id.
    OpenAgent(String),
    /// Open the tray window.
    Open,
    AddFolder,
    StartAtLogon,
    SwitchOver,
    Quit,
}

/// The command a menu item id names; `None` for anything else.
pub fn parse(id: &str) -> Option<MenuCommand> {
    if let Some(instance) = id.strip_prefix(AGENT_PREFIX) {
        return (!instance.is_empty()).then(|| MenuCommand::OpenAgent(instance.to_owned()));
    }
    match id {
        OPEN => Some(MenuCommand::Open),
        ADD_FOLDER => Some(MenuCommand::AddFolder),
        START_AT_LOGON => Some(MenuCommand::StartAtLogon),
        SWITCH_OVER => Some(MenuCommand::SwitchOver),
        QUIT => Some(MenuCommand::Quit),
        _ => None,
    }
}

/// The item id of the agent with the instance id `instance`.
pub fn agent_id(instance: &str) -> String {
    format!("{AGENT_PREFIX}{instance}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub items: Vec<Item>,
}

/// A menu item. Labels are ready for a native menu: `&` marks the mnemonic and a literal
/// ampersand is `&&`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// An agent's line, with its state's badge.
    Agent {
        id: String,
        label: String,
        icon: IconState,
    },
    Action {
        id: String,
        label: String,
        enabled: bool,
    },
    Check {
        id: String,
        label: String,
        checked: bool,
    },
    Separator,
    /// A disabled line of text.
    Info(String),
}

/// `text` with every `&` doubled, so that a native menu shows it as it is.
pub fn escape(text: &str) -> String {
    text.replace('&', "&&")
}

/// The tray menu: a line per agent ("<name>: <state>", or "<name>: <menu line>" when
/// `menu_lines` has one for its instance id, or "<name>: <call>" ([`call_short`]) while a
/// voice call runs or rings), then "Open…", "Add agent folder…", "Start at logon", the
/// switch-over when offered, and "Quit". A `config_error` (tray.json unreadable) comes
/// first.
pub fn build(
    views: &[InstanceView],
    tray_autostart: bool,
    offer: &Offer,
    config_error: Option<&str>,
    menu_lines: &HashMap<String, String>,
) -> Menu {
    let mut items = Vec::new();
    if let Some(error) = config_error {
        items.push(Item::Info(escape(&format!("tray.json: {error}"))));
        items.push(Item::Separator);
    }
    if views.is_empty() {
        items.push(Item::Info("No agents".into()));
    }
    items.extend(views.iter().map(|view| {
        let line = match &view.call {
            Some(call) => call_short(call).to_owned(),
            None => menu_lines
                .get(&view.id)
                .cloned()
                .unwrap_or_else(|| label(&view.state)),
        };
        Item::Agent {
            id: agent_id(&view.id),
            label: escape(&format!("{}: {line}", view.name)),
            icon: icon_for_view(&view.state, view.connected),
        }
    }));
    items.push(Item::Separator);
    let action = |id: &str, label: &str| Item::Action {
        id: id.into(),
        label: label.into(),
        enabled: true,
    };
    items.push(action(OPEN, "&Open…"));
    items.push(action(ADD_FOLDER, "&Add agent folder…"));
    items.push(Item::Check {
        id: START_AT_LOGON.into(),
        label: "Start at &logon".into(),
        checked: tray_autostart,
    });
    if let Offer::SwitchOver(_) = offer {
        items.push(action(SWITCH_OVER, "&Switch from Scheduled Task…"));
    }
    items.push(Item::Separator);
    items.push(action(QUIT, "&Quit"));
    Menu { items }
}

/// Whether `action` is available for the agent in `view`.
pub fn enabled(view: &InstanceView, action: AgentAction) -> bool {
    let state = &view.state;
    if *state == AgentState::NotSetUp {
        return action == AgentAction::Start;
    }
    let running = matches!(
        state,
        AgentState::Idle
            | AgentState::Working
            | AgentState::Waiting(_)
            | AgentState::Starting
            | AgentState::Reconnecting
            | AgentState::Restarting
            | AgentState::Paused
            | AgentState::External { .. }
    );
    let live = view.connected && *state != AgentState::Stopping;
    let call = view.call.as_ref().filter(|_| live);
    let answered = call.filter(|call| call.phase != CallPhase::Ringing);
    match action {
        AgentAction::Start => matches!(
            state,
            AgentState::Stopped | AgentState::GaveUp { .. } | AgentState::Failed { .. }
        ),
        AgentAction::Stop => running || matches!(state, AgentState::Failed { .. }),
        AgentAction::RestartBackend | AgentAction::Reconnect => live,
        AgentAction::Pause => live && *state != AgentState::Paused,
        AgentAction::Resume => live && *state == AgentState::Paused,
        AgentAction::Mute => answered.is_some_and(|call| !call.muted),
        AgentAction::Unmute => answered.is_some_and(|call| call.muted),
        AgentAction::Hangup => call.is_some(),
    }
}

/// The bridge's log file in `dir`.
pub fn log_path(dir: &Path) -> PathBuf {
    dir.join("logs").join("bridge.log")
}

/// The tray tooltip for `views` (see [`crate::state::tooltip`]).
pub fn tooltip(views: &[InstanceView]) -> String {
    let agents: Vec<(String, AgentState)> = views
        .iter()
        .map(|view| (view.name.clone(), view.state.clone()))
        .collect();
    state_tooltip(&agents)
}

/// The tray icon's badge for `views`: the most urgent agent's; `None` without agents.
pub fn icon(views: &[InstanceView]) -> Option<IconState> {
    worst(
        views
            .iter()
            .map(|view| icon_for_view(&view.state, view.connected)),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::history::History;
    use crate::schedtask::TaskFor;
    use push_bridge_control::{CallDirection, CallInfo};
    use std::time::Duration;

    /// An instance view for tests (also used by the window's tests).
    pub(crate) fn view(name: &str, state: AgentState, connected: bool) -> InstanceView {
        InstanceView {
            dir: PathBuf::from(format!("bridges/{name}")),
            id: format!("{name}-id"),
            name: name.into(),
            state,
            started_by_tray: true,
            connected,
            autostart: true,
            task: None,
            status: None,
            history: History::new(),
            last_exit: None,
            last_error: None,
            peek_changes: 0,
            control: None,
            refusal: None,
            call: None,
        }
    }

    /// A call for tests (also used by the window's tests).
    pub(crate) fn call(phase: CallPhase, muted: bool) -> CallInfo {
        CallInfo {
            phase,
            direction: CallDirection::Incoming,
            speaking: false,
            listening: true,
            muted,
            since: 1,
        }
    }

    fn no_lines() -> HashMap<String, String> {
        HashMap::new()
    }

    /// The actions enabled for each state, as `AgentAction::as_str` names.
    pub(crate) fn enabled_cases() -> [(AgentState, bool, &'static [&'static str]); 9] {
        [
            (
                AgentState::Idle,
                true,
                &["stop", "restart_backend", "reconnect", "pause"],
            ),
            (
                AgentState::Paused,
                true,
                &["stop", "restart_backend", "reconnect", "resume"],
            ),
            (
                AgentState::External { task: false },
                true,
                &["stop", "restart_backend", "reconnect", "pause"],
            ),
            (AgentState::Starting, false, &["stop"]),
            (AgentState::Stopping, true, &[]),
            (AgentState::Stopped, false, &["start"]),
            (
                AgentState::Failed {
                    code: Some(1),
                    retry_in: Some(Duration::from_secs(4)),
                },
                false,
                &["start", "stop"],
            ),
            (AgentState::GaveUp { code: Some(1) }, false, &["start"]),
            (AgentState::NotSetUp, false, &["start"]),
        ]
    }

    #[test]
    fn actions_round_trip_by_name() {
        for action in AgentAction::ALL {
            assert_eq!(AgentAction::parse(action.as_str()), Some(action));
        }
        assert_eq!(AgentAction::parse("open_log"), None);
        assert_eq!(AgentAction::parse(""), None);
        assert_eq!(AgentAction::Pause.label(), "Pause for upgrade");
        assert_eq!(AgentAction::RestartBackend.label(), "Restart Coda");
        assert_eq!(AgentAction::Mute.label(), "Mute agent");
        assert_eq!(AgentAction::Unmute.label(), "Unmute agent");
        assert_eq!(AgentAction::Hangup.label(), "Hang up");
        assert_eq!(AgentAction::ALL[..6], AgentAction::AGENT);
    }

    #[test]
    fn ids_round_trip() {
        assert_eq!(
            parse(&agent_id("00112233aabbccdd")),
            Some(MenuCommand::OpenAgent("00112233aabbccdd".into()))
        );
        for (id, command) in [
            (OPEN, MenuCommand::Open),
            (ADD_FOLDER, MenuCommand::AddFolder),
            (START_AT_LOGON, MenuCommand::StartAtLogon),
            (SWITCH_OVER, MenuCommand::SwitchOver),
            (QUIT, MenuCommand::Quit),
        ] {
            assert_eq!(parse(id), Some(command), "{id}");
        }
        for id in ["", "agent:", "stop:x", "details:x", "Quit", "explode"] {
            assert_eq!(parse(id), None, "{id:?}");
        }
    }

    #[test]
    fn the_menu_has_a_line_per_agent_then_the_tray_items() {
        let views = [
            view("a", AgentState::Idle, true),
            view("b", AgentState::Stopped, false),
        ];
        let menu = build(&views, true, &Offer::None, None, &no_lines());
        assert_eq!(
            menu.items,
            [
                Item::Agent {
                    id: "agent:a-id".into(),
                    label: "a: connected, idle".into(),
                    icon: IconState::Normal,
                },
                Item::Agent {
                    id: "agent:b-id".into(),
                    label: "b: stopped".into(),
                    icon: IconState::Error,
                },
                Item::Separator,
                Item::Action {
                    id: OPEN.into(),
                    label: "&Open…".into(),
                    enabled: true,
                },
                Item::Action {
                    id: ADD_FOLDER.into(),
                    label: "&Add agent folder…".into(),
                    enabled: true,
                },
                Item::Check {
                    id: START_AT_LOGON.into(),
                    label: "Start at &logon".into(),
                    checked: true,
                },
                Item::Separator,
                Item::Action {
                    id: QUIT.into(),
                    label: "&Quit".into(),
                    enabled: true,
                },
            ]
        );
        for item in &menu.items {
            if let Item::Agent { id, .. } | Item::Action { id, .. } | Item::Check { id, .. } = item
            {
                assert!(parse(id).is_some(), "{id}");
            }
        }
    }

    #[test]
    fn a_config_error_no_agents_and_the_switch_over_show() {
        let offer = Offer::SwitchOver(vec![TaskFor {
            task: "t".into(),
            path: "\\".into(),
            dir: PathBuf::from("bridges/a"),
        }]);
        let menu = build(&[], false, &offer, Some("bad & JSON"), &no_lines());
        assert_eq!(menu.items[0], Item::Info("tray.json: bad && JSON".into()));
        assert_eq!(menu.items[1], Item::Separator);
        assert_eq!(menu.items[2], Item::Info("No agents".into()));
        assert!(menu.items.contains(&Item::Action {
            id: SWITCH_OVER.into(),
            label: "&Switch from Scheduled Task…".into(),
            enabled: true,
        }));
        assert!(menu.items.contains(&Item::Check {
            id: START_AT_LOGON.into(),
            label: "Start at &logon".into(),
            checked: false,
        }));
        assert_eq!(
            menu.items.last(),
            Some(&Item::Action {
                id: QUIT.into(),
                label: "&Quit".into(),
                enabled: true,
            })
        );
    }

    #[test]
    fn agent_lines_use_the_menu_line_and_escape_ampersands() {
        let views = [
            view("R&D", AgentState::Waiting(Some("approval".into())), true),
            view("Ops", AgentState::Working, true),
            view("Away", AgentState::Idle, false),
        ];
        let lines = HashMap::from([("Ops-id".to_owned(), "Done: tests & lint pass".to_owned())]);
        let menu = build(&views, false, &Offer::None, None, &lines);
        assert_eq!(
            menu.items[..3],
            [
                Item::Agent {
                    id: "agent:R&D-id".into(),
                    label: "R&&D: waiting for you (approval)".into(),
                    icon: IconState::Waiting,
                },
                Item::Agent {
                    id: "agent:Ops-id".into(),
                    label: "Ops: Done: tests && lint pass".into(),
                    icon: IconState::Busy,
                },
                Item::Agent {
                    id: "agent:Away-id".into(),
                    label: "Away: connected, idle".into(),
                    icon: IconState::Disconnected,
                },
            ]
        );
        assert_eq!(
            parse("agent:R&D-id"),
            Some(MenuCommand::OpenAgent("R&D-id".into()))
        );
    }

    #[test]
    fn actions_are_enabled_by_state() {
        for (state, connected, expected) in enabled_cases() {
            let view = view("a", state.clone(), connected);
            let enabled: Vec<&str> = AgentAction::ALL
                .into_iter()
                .filter(|action| enabled(&view, *action))
                .map(AgentAction::as_str)
                .collect();
            assert_eq!(enabled, expected, "{state:?}");
        }
    }

    #[test]
    fn call_actions_are_enabled_by_the_call() {
        let call_actions = [AgentAction::Mute, AgentAction::Unmute, AgentAction::Hangup];
        let enabled_for = |view: &InstanceView| -> Vec<&str> {
            call_actions
                .into_iter()
                .filter(|action| enabled(view, *action))
                .map(AgentAction::as_str)
                .collect()
        };
        let cases: [(Option<CallInfo>, &[&str]); 6] = [
            (None, &[]),
            (Some(call(CallPhase::Ringing, false)), &["hangup"]),
            (
                Some(call(CallPhase::Connecting, false)),
                &["mute", "hangup"],
            ),
            (Some(call(CallPhase::InCall, false)), &["mute", "hangup"]),
            (Some(call(CallPhase::InCall, true)), &["unmute", "hangup"]),
            (
                Some(call(CallPhase::Connecting, true)),
                &["unmute", "hangup"],
            ),
        ];
        for (call, expected) in cases {
            for state in [AgentState::Idle, AgentState::Working, AgentState::Paused] {
                let mut view = view("a", state, true);
                view.call = call.clone();
                assert_eq!(enabled_for(&view), expected, "{call:?}");
                // Not while the control channel is down or the bridge stops.
                view.connected = false;
                assert!(enabled_for(&view).is_empty(), "{call:?}");
                view.connected = true;
                view.state = AgentState::Stopping;
                assert!(enabled_for(&view).is_empty(), "{call:?}");
            }
        }
    }

    #[test]
    fn a_call_shows_in_the_agent_line() {
        let mut calling = view("Coda", AgentState::Working, true);
        calling.call = Some(CallInfo {
            direction: CallDirection::Outgoing,
            ..call(CallPhase::Ringing, false)
        });
        let mut talking = view("Ops", AgentState::Idle, true);
        talking.call = Some(call(CallPhase::InCall, true));
        let lines = HashMap::from([("Ops-id".to_owned(), "Done".to_owned())]);
        let menu = build(&[calling, talking], false, &Offer::None, None, &lines);
        assert_eq!(
            menu.items[..2],
            [
                Item::Agent {
                    id: "agent:Coda-id".into(),
                    label: "Coda: calling you".into(),
                    icon: IconState::Busy,
                },
                Item::Agent {
                    id: "agent:Ops-id".into(),
                    label: "Ops: on a call".into(),
                    icon: IconState::Normal,
                },
            ]
        );
    }

    #[test]
    fn tooltip_and_icon_come_from_the_views() {
        let views = [
            view(
                "Coda (coda)",
                AgentState::Waiting(Some("approval".into())),
                true,
            ),
            view("Other (other)", AgentState::Working, true),
        ];
        assert_eq!(
            tooltip(&views),
            "Coda (coda): waiting for you (approval)\nOther (other): working"
        );
        assert_eq!(icon(&views), Some(IconState::Waiting));
        assert_eq!(icon(&[]), None);
        let offline = [view("Coda (coda)", AgentState::Idle, false)];
        assert_eq!(icon(&offline), Some(IconState::Disconnected));
        assert_eq!(tooltip(&[]), "push-agent-tray\nNo agents");
    }

    #[test]
    fn the_log_is_in_the_logs_folder() {
        assert_eq!(
            log_path(Path::new("d")),
            Path::new("d").join("logs").join("bridge.log")
        );
    }
}
