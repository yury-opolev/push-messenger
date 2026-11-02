//! The tray menu as data: the shell turns it into a native menu, and menu ids back into
//! actions with [`parse_id`].

use std::path::{Path, PathBuf};

use crate::schedtask::Offer;
use crate::state::{AgentState, IconState, icon_for, label, tooltip as state_tooltip, worst};
use crate::supervisor::InstanceView;

/// "Add agent folder…".
pub const ADD_FOLDER: &str = "add_folder";
/// The "Start at logon" check item.
pub const START_AT_LOGON: &str = "start_at_logon";
/// "Switch from Scheduled Task to tray…".
pub const SWITCH_OVER: &str = "switch_over";
pub const QUIT: &str = "quit";

/// An agent's menu action; its item id is `"<action>:<instance id>"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuAction {
    Start,
    Stop,
    RestartBackend,
    Reconnect,
    Pause,
    Resume,
    OpenLog,
    OpenFolder,
    Details,
}

impl MenuAction {
    pub const ALL: [MenuAction; 9] = [
        MenuAction::Start,
        MenuAction::Stop,
        MenuAction::RestartBackend,
        MenuAction::Reconnect,
        MenuAction::Pause,
        MenuAction::Resume,
        MenuAction::OpenLog,
        MenuAction::OpenFolder,
        MenuAction::Details,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            MenuAction::Start => "start",
            MenuAction::Stop => "stop",
            MenuAction::RestartBackend => "restart_backend",
            MenuAction::Reconnect => "reconnect",
            MenuAction::Pause => "pause",
            MenuAction::Resume => "resume",
            MenuAction::OpenLog => "open_log",
            MenuAction::OpenFolder => "open_folder",
            MenuAction::Details => "details",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MenuAction::Start => "Start",
            MenuAction::Stop => "Stop",
            MenuAction::RestartBackend => "Restart Coda",
            MenuAction::Reconnect => "Reconnect",
            MenuAction::Pause => "Pause Coda for upgrade",
            MenuAction::Resume => "Resume",
            MenuAction::OpenLog => "Open log",
            MenuAction::OpenFolder => "Open folder",
            MenuAction::Details => "Details…",
        }
    }

    /// The item id for `instance`.
    pub fn id(self, instance: &str) -> String {
        format!("{}:{instance}", self.as_str())
    }
}

/// An agent item's action and instance id; `None` for the top-level ids and anything else.
pub fn parse_id(id: &str) -> Option<(MenuAction, String)> {
    let (action, instance) = id.split_once(':')?;
    if instance.is_empty() {
        return None;
    }
    let action = MenuAction::ALL
        .into_iter()
        .find(|candidate| candidate.as_str() == action)?;
    Some((action, instance.to_owned()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
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
    Submenu {
        label: String,
        items: Vec<Item>,
    },
    /// A disabled line of text.
    Info(String),
}

/// The tray menu: a submenu per agent, then "Add agent folder…", "Start at logon", the
/// switch-over when offered, and "Quit". A `config_error` (tray.json unreadable) comes
/// first.
pub fn build(
    views: &[InstanceView],
    tray_autostart: bool,
    offer: &Offer,
    config_error: Option<&str>,
) -> Menu {
    let mut items = Vec::new();
    if let Some(error) = config_error {
        items.push(Item::Info(format!("tray.json: {error}")));
        items.push(Item::Separator);
    }
    if views.is_empty() {
        items.push(Item::Info("No agents".into()));
    }
    items.extend(views.iter().map(agent_menu));
    items.push(Item::Separator);
    items.push(Item::Action {
        id: ADD_FOLDER.into(),
        label: "Add agent folder…".into(),
        enabled: true,
    });
    items.push(Item::Check {
        id: START_AT_LOGON.into(),
        label: "Start at logon".into(),
        checked: tray_autostart,
    });
    if let Offer::SwitchOver(_) = offer {
        items.push(Item::Action {
            id: SWITCH_OVER.into(),
            label: "Switch from Scheduled Task to tray…".into(),
            enabled: true,
        });
    }
    items.push(Item::Separator);
    items.push(Item::Action {
        id: QUIT.into(),
        label: "Quit".into(),
        enabled: true,
    });
    Menu { items }
}

fn agent_menu(view: &InstanceView) -> Item {
    let session = view
        .status
        .as_ref()
        .and_then(|status| status.session.clone())
        .unwrap_or_else(|| "none".into());
    let mut items = vec![
        Item::Info(label(&view.state)),
        Item::Info(format!("Folder: {}", view.dir.display())),
        Item::Info(format!("Session: {session}")),
        Item::Separator,
    ];
    let action = |action: MenuAction| Item::Action {
        id: action.id(&view.id),
        label: action.label().into(),
        enabled: enabled(view, action),
    };
    items.extend(
        [
            MenuAction::Start,
            MenuAction::Stop,
            MenuAction::RestartBackend,
            MenuAction::Reconnect,
            MenuAction::Pause,
            MenuAction::Resume,
        ]
        .map(action),
    );
    items.push(Item::Separator);
    items.extend(
        [
            MenuAction::OpenLog,
            MenuAction::OpenFolder,
            MenuAction::Details,
        ]
        .map(action),
    );
    Item::Submenu {
        label: view.name.clone(),
        items,
    }
}

/// Whether `action` is available for the agent in `view`.
pub fn enabled(view: &InstanceView, action: MenuAction) -> bool {
    let state = &view.state;
    if *state == AgentState::NotSetUp {
        return matches!(
            action,
            MenuAction::Start | MenuAction::OpenFolder | MenuAction::Details
        );
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
            | AgentState::External
    );
    let live = view.connected && *state != AgentState::Stopping;
    match action {
        MenuAction::Start => matches!(
            state,
            AgentState::Stopped | AgentState::GaveUp { .. } | AgentState::Failed { .. }
        ),
        MenuAction::Stop => running || matches!(state, AgentState::Failed { .. }),
        MenuAction::RestartBackend | MenuAction::Reconnect => live,
        MenuAction::Pause => live && *state != AgentState::Paused,
        MenuAction::Resume => live && *state == AgentState::Paused,
        MenuAction::OpenLog | MenuAction::OpenFolder | MenuAction::Details => true,
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

/// The tray icon for `views`: the most urgent agent's.
pub fn icon(views: &[InstanceView]) -> IconState {
    worst(views.iter().map(|view| icon_for(&view.state)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::History;
    use crate::schedtask::TaskFor;
    use push_bridge_control::Status;
    use std::time::Duration;

    fn view(name: &str, state: AgentState, connected: bool) -> InstanceView {
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
        }
    }

    fn submenu(item: &Item) -> (&str, &[Item]) {
        match item {
            Item::Submenu { label, items } => (label, items),
            other => panic!("not a submenu: {other:?}"),
        }
    }

    fn enabled_ids(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .filter_map(|item| match item {
                Item::Action {
                    id, enabled: true, ..
                } => Some(id.split(':').next().unwrap().to_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn ids_round_trip_and_top_level_ids_are_not_agent_actions() {
        for action in MenuAction::ALL {
            let id = action.id("00112233aabbccdd");
            assert_eq!(parse_id(&id), Some((action, "00112233aabbccdd".to_owned())));
        }
        for id in [
            ADD_FOLDER,
            START_AT_LOGON,
            SWITCH_OVER,
            QUIT,
            "",
            "stop:",
            "explode:x",
            ":x",
        ] {
            assert_eq!(parse_id(id), None, "{id:?}");
        }
    }

    #[test]
    fn the_top_level_has_agents_then_the_tray_items() {
        let views = [
            view("a", AgentState::Idle, true),
            view("b", AgentState::Stopped, false),
        ];
        let menu = build(&views, true, &Offer::None, None);
        assert_eq!(submenu(&menu.items[0]).0, "a");
        assert_eq!(submenu(&menu.items[1]).0, "b");
        assert_eq!(
            menu.items[2..],
            [
                Item::Separator,
                Item::Action {
                    id: ADD_FOLDER.into(),
                    label: "Add agent folder…".into(),
                    enabled: true,
                },
                Item::Check {
                    id: START_AT_LOGON.into(),
                    label: "Start at logon".into(),
                    checked: true,
                },
                Item::Separator,
                Item::Action {
                    id: QUIT.into(),
                    label: "Quit".into(),
                    enabled: true,
                },
            ]
        );
        let offer = Offer::SwitchOver(vec![TaskFor {
            task: "t".into(),
            path: "\\".into(),
            dir: PathBuf::from("bridges/a"),
        }]);
        let menu = build(&[], false, &offer, Some("bad JSON"));
        assert_eq!(menu.items[0], Item::Info("tray.json: bad JSON".into()));
        assert_eq!(menu.items[2], Item::Info("No agents".into()));
        assert!(menu.items.contains(&Item::Action {
            id: SWITCH_OVER.into(),
            label: "Switch from Scheduled Task to tray…".into(),
            enabled: true,
        }));
        assert!(menu.items.contains(&Item::Check {
            id: START_AT_LOGON.into(),
            label: "Start at logon".into(),
            checked: false,
        }));
    }

    #[test]
    fn an_agent_submenu_shows_state_folder_and_session() {
        let mut idle = view("a", AgentState::Waiting(Some("approval".into())), true);
        idle.status = Some(Status {
            session: Some("s-42".into()),
            ..Status::default()
        });
        let menu = build(&[idle], false, &Offer::None, None);
        let (_, items) = submenu(&menu.items[0]);
        assert_eq!(items[0], Item::Info("waiting for you (approval)".into()));
        assert_eq!(
            items[1],
            Item::Info(format!("Folder: {}", PathBuf::from("bridges/a").display()))
        );
        assert_eq!(items[2], Item::Info("Session: s-42".into()));
        assert_eq!(items[3], Item::Separator);
        let labels: Vec<&str> = items
            .iter()
            .filter_map(|item| match item {
                Item::Action { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels,
            [
                "Start",
                "Stop",
                "Restart Coda",
                "Reconnect",
                "Pause Coda for upgrade",
                "Resume",
                "Open log",
                "Open folder",
                "Details…"
            ]
        );
        let stopped = build(
            &[view("b", AgentState::Stopped, false)],
            false,
            &Offer::None,
            None,
        );
        assert_eq!(
            submenu(&stopped.items[0]).1[2],
            Item::Info("Session: none".into())
        );
    }

    #[test]
    fn actions_are_enabled_by_state() {
        let cases: [(AgentState, bool, &[&str]); 9] = [
            (
                AgentState::Idle,
                true,
                &[
                    "stop",
                    "restart_backend",
                    "reconnect",
                    "pause",
                    "open_log",
                    "open_folder",
                    "details",
                ],
            ),
            (
                AgentState::Paused,
                true,
                &[
                    "stop",
                    "restart_backend",
                    "reconnect",
                    "resume",
                    "open_log",
                    "open_folder",
                    "details",
                ],
            ),
            (
                AgentState::External,
                true,
                &[
                    "stop",
                    "restart_backend",
                    "reconnect",
                    "pause",
                    "open_log",
                    "open_folder",
                    "details",
                ],
            ),
            (
                AgentState::Starting,
                false,
                &["stop", "open_log", "open_folder", "details"],
            ),
            (
                AgentState::Stopping,
                true,
                &["open_log", "open_folder", "details"],
            ),
            (
                AgentState::Stopped,
                false,
                &["start", "open_log", "open_folder", "details"],
            ),
            (
                AgentState::Failed {
                    code: Some(1),
                    retry_in: Some(Duration::from_secs(4)),
                },
                false,
                &["start", "stop", "open_log", "open_folder", "details"],
            ),
            (
                AgentState::GaveUp { code: Some(1) },
                false,
                &["start", "open_log", "open_folder", "details"],
            ),
            (
                AgentState::NotSetUp,
                false,
                &["start", "open_folder", "details"],
            ),
        ];
        for (state, connected, expected) in cases {
            let menu = build(
                &[view("a", state.clone(), connected)],
                false,
                &Offer::None,
                None,
            );
            let (_, items) = submenu(&menu.items[0]);
            assert_eq!(enabled_ids(items), expected, "{state:?}");
        }
    }

    #[test]
    fn a_not_set_up_agent_says_so() {
        let menu = build(
            &[view("a", AgentState::NotSetUp, false)],
            false,
            &Offer::None,
            None,
        );
        let (_, items) = submenu(&menu.items[0]);
        assert_eq!(
            items[0],
            Item::Info("Not set up: run keygen and enroll".into())
        );
        assert_eq!(enabled_ids(items), ["start", "open_folder", "details"]);
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
        assert_eq!(icon(&views), IconState::Waiting);
        assert_eq!(icon(&[]), IconState::Normal);
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
