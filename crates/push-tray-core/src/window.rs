//! The tray window's view models, which its page renders: the agent list
//! ([`window_view`]), the selected agent ([`agent_view`]), and the notifications
//! ([`notice_text`]). Content-free like everything else: states, names, counts and error
//! codes only (the chat peek is [`crate::peek`]).

use serde::Serialize;

use push_bridge_control::AdapterStatus;

use crate::menu::{AgentAction, enabled};
use crate::policy::MAX_EXITS;
use crate::state::{IconState, call_label, call_short, icon_for_view, label};
use crate::supervisor::{InstanceView, Notice};
use crate::tray_config::{self, PEEK_MESSAGES_DEFAULT};

/// The tray's own settings, shown in the window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalSettings {
    pub start_at_logon: bool,
    /// "Switch from Scheduled Task…" is offered.
    pub switch_offered: bool,
    /// Show each agent's last line in the tray menu.
    pub menu_peek: bool,
    pub reduce_motion: bool,
    /// Why tray.json could not be read.
    pub config_error: Option<String>,
}

/// The window's agent list and the tray's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowView {
    pub agents: Vec<AgentRow>,
    pub global: GlobalSettings,
}

/// An agent in the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRow {
    /// The instance id.
    pub id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub handle: Option<String>,
    /// See [`badge_key`].
    pub badge: &'static str,
    /// The state's label.
    pub state: String,
    /// The voice call in a few words ([`call_short`]), while one runs or rings.
    pub call: Option<String>,
    pub folder: String,
}

/// The selected agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentView {
    pub id: String,
    pub name: String,
    pub badge: &'static str,
    pub state: String,
    /// Start, Stop, Restart Coda, Reconnect, Pause for upgrade, Resume.
    pub actions: Vec<ActionView>,
    /// The voice call ([`call_label`]), while one runs or rings.
    pub call: Option<String>,
    /// The voice adapter ([`adapter_label`]), when the bridge has one.
    pub voice_adapter: Option<String>,
    /// The call's buttons: "Mute agent" ("Unmute agent" while muted), then "Hang up".
    pub call_actions: Vec<ActionView>,
    pub folder: String,
    pub session: Option<String>,
    pub coda_version: Option<String>,
    /// While connected: how long the bridge runs (see [`uptime`]).
    pub uptime: Option<String>,
    pub running_tasks: Option<usize>,
    /// The tray's own error, else the bridge's last error code.
    pub last_error: Option<String>,
    pub last_exit: Option<i32>,
    /// Newest first, at most [`crate::history::HISTORY_CAP`].
    pub history: Vec<HistoryEntry>,
    pub settings: AgentSettings,
}

/// An action button.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionView {
    /// [`AgentAction::as_str`].
    pub action: &'static str,
    pub label: &'static str,
    pub enabled: bool,
    /// Why the bridge refused this action the last time (shown next to the button).
    pub refused: Option<String>,
}

/// A state the agent was in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// Unix milliseconds.
    pub at: i64,
    pub state: String,
}

/// The agent's settings from tray.json.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSettings {
    pub autostart: bool,
    pub show_peek: bool,
    /// Clamped to [`tray_config::PEEK_MESSAGES_MIN`]`..=`[`tray_config::PEEK_MESSAGES_MAX`].
    pub peek_messages: u32,
    /// The bridge's config.json.
    pub config_path: String,
}

/// The badge's key for the page's styles.
pub fn badge_key(state: IconState) -> &'static str {
    match state {
        IconState::Normal => "green",
        IconState::Busy => "blue",
        IconState::Transition => "blue-pulse",
        IconState::Paused => "purple",
        IconState::Waiting => "amber",
        IconState::Disconnected => "grey",
        IconState::Error => "red",
    }
}

fn badge(view: &InstanceView) -> &'static str {
    badge_key(icon_for_view(&view.state, view.connected))
}

/// The agent list for `views`, with the tray's settings.
pub fn window_view(views: &[InstanceView], global: GlobalSettings) -> WindowView {
    WindowView {
        agents: views
            .iter()
            .map(|view| {
                let agent = view
                    .status
                    .as_ref()
                    .and_then(|status| status.agent.as_ref());
                AgentRow {
                    id: view.id.clone(),
                    name: view.name.clone(),
                    display_name: agent.map(|agent| agent.display_name.clone()),
                    handle: agent.map(|agent| agent.handle.clone()),
                    badge: badge(view),
                    state: label(&view.state),
                    call: view.call.as_ref().map(|call| call_short(call).to_owned()),
                    folder: view.dir.display().to_string(),
                }
            })
            .collect(),
        global,
    }
}

/// The agent in `view` at `now_ms` (Unix milliseconds), with its settings from tray.json's
/// `instance` (the defaults, and the view's autostart, without one).
pub fn agent_view(
    view: &InstanceView,
    instance: Option<&tray_config::Instance>,
    now_ms: i64,
) -> AgentView {
    let status = view.status.as_ref();
    let backend = status.and_then(|status| status.backend.as_ref());
    let action_view = |action: AgentAction| ActionView {
        action: action.as_str(),
        label: action.label(),
        enabled: enabled(view, action),
        refused: view
            .refusal
            .as_ref()
            .filter(|refusal| refusal.action == action)
            .map(|refusal| refusal.message.clone()),
    };
    let actions = AgentAction::AGENT.into_iter().map(action_view).collect();
    let muted = view.call.as_ref().is_some_and(|call| call.muted);
    let toggle = if muted {
        AgentAction::Unmute
    } else {
        AgentAction::Mute
    };
    let call_actions = [toggle, AgentAction::Hangup]
        .into_iter()
        .map(action_view)
        .collect();
    let started_at = status
        .map(|status| status.started_at)
        .filter(|started_at| view.connected && *started_at > 0);
    AgentView {
        id: view.id.clone(),
        name: view.name.clone(),
        badge: badge(view),
        state: label(&view.state),
        actions,
        call: view.call.as_ref().map(call_label),
        voice_adapter: status
            .and_then(|status| status.adapter.as_ref())
            .map(adapter_label),
        call_actions,
        folder: view.dir.display().to_string(),
        session: status.and_then(|status| status.session.clone()),
        coda_version: backend.and_then(|backend| backend.version.clone()),
        uptime: started_at.map(|started_at| uptime(now_ms - started_at)),
        running_tasks: backend.map(|backend| backend.running_tasks),
        last_error: view.last_error.clone().or_else(|| {
            status
                .and_then(|status| status.last_error.as_ref())
                .map(|error| error.code.clone())
        }),
        last_exit: view.last_exit,
        history: view
            .history
            .iter()
            .rev()
            .map(|(at, state)| HistoryEntry {
                at: *at,
                state: state.clone(),
            })
            .collect(),
        settings: AgentSettings {
            autostart: instance.map_or(view.autostart, |instance| instance.autostart),
            show_peek: instance.is_none_or(|instance| instance.show_peek),
            peek_messages: instance
                .map_or(PEEK_MESSAGES_DEFAULT, |instance| instance.peek_messages()),
            config_path: view.dir.join("config.json").display().to_string(),
        },
    }
}

/// The voice adapter for people: "whisper 1.2.3 (ready)", "whisper 1.2.3 (ready, cannot
/// listen)", "starting", "backing off (<reason>)", "off".
pub fn adapter_label(adapter: &AdapterStatus) -> String {
    match adapter.state.as_str() {
        "ready" => {
            let listens = adapter
                .capabilities
                .as_ref()
                .is_some_and(|capabilities| capabilities.iter().any(|name| name == "stt"));
            format!(
                "{} {} ({})",
                adapter.name.as_deref().unwrap_or("?"),
                adapter.version.as_deref().unwrap_or("?"),
                if listens {
                    "ready"
                } else {
                    "ready, cannot listen"
                }
            )
        }
        "starting" | "handshaking" => "starting".to_owned(),
        "backoff" => match &adapter.reason {
            Some(reason) => format!("backing off ({reason})"),
            None => "backing off".to_owned(),
        },
        other => other.to_owned(),
    }
}

/// `elapsed_ms` for people: "45 s", "12 min", "2 h 5 min", "3 d 4 h" (a smaller unit of 0
/// is left out: "2 h").
pub fn uptime(elapsed_ms: i64) -> String {
    let seconds = elapsed_ms.max(0) / 1000;
    let (minutes, hours, days) = (seconds / 60, seconds / 3600, seconds / 86_400);
    let pair = |big: i64, big_unit: &str, small: i64, small_unit: &str| {
        if small == 0 {
            format!("{big} {big_unit}")
        } else {
            format!("{big} {big_unit} {small} {small_unit}")
        }
    };
    if seconds < 60 {
        format!("{seconds} s")
    } else if minutes < 60 {
        format!("{minutes} min")
    } else if hours < 24 {
        pair(hours, "h", minutes % 60, "min")
    } else {
        pair(days, "d", hours % 24, "h")
    }
}

/// The notification for `notice`.
pub fn notice_text(notice: &Notice) -> String {
    match notice {
        Notice::GaveUp { name, code, .. } => {
            let exit = code.map_or_else(String::new, |code| format!(" (exit {code})"));
            format!(
                "{name} stopped after {MAX_EXITS} failures{exit}. Open its log from the tray window."
            )
        }
        Notice::NotSetUp { name, .. } => {
            format!("{name} is not set up: run keygen and enroll.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::tests::{call, enabled_cases, view};
    use crate::state::AgentState;
    use crate::supervisor::Refusal;
    use push_bridge_control::{AgentInfo, BackendInfo, CallPhase, LastError, Status};
    use std::path::PathBuf;

    fn status() -> Status {
        Status {
            agent: Some(AgentInfo {
                display_name: "Coda".into(),
                handle: "coda".into(),
            }),
            session: Some("s-42".into()),
            backend: Some(BackendInfo {
                name: "coda".into(),
                version: Some("1.2.3".into()),
                running: true,
                running_tasks: 2,
            }),
            last_error: Some(LastError {
                code: "backend_crashed".into(),
                at: 5,
            }),
            started_at: 1_000,
            ..Status::default()
        }
    }

    #[test]
    fn badges_have_keys() {
        let keys: Vec<&str> = IconState::ALL.into_iter().map(badge_key).collect();
        assert_eq!(
            keys,
            [
                "green",
                "blue",
                "blue-pulse",
                "purple",
                "amber",
                "grey",
                "red"
            ]
        );
    }

    #[test]
    fn rows_come_from_the_views() {
        let mut coda = view("Coda (coda)", AgentState::Waiting(None), true);
        coda.status = Some(status());
        let mut b = view("b", AgentState::Idle, true);
        b.call = Some(call(CallPhase::InCall, false));
        let views = [coda, b];
        let global = GlobalSettings {
            start_at_logon: true,
            menu_peek: true,
            config_error: Some("bad".into()),
            ..GlobalSettings::default()
        };
        let window = window_view(&views, global.clone());
        assert_eq!(window.global, global);
        assert_eq!(
            window.agents,
            [
                AgentRow {
                    id: "Coda (coda)-id".into(),
                    name: "Coda (coda)".into(),
                    display_name: Some("Coda".into()),
                    handle: Some("coda".into()),
                    badge: "amber",
                    state: "waiting for you".into(),
                    call: None,
                    folder: PathBuf::from("bridges/Coda (coda)").display().to_string(),
                },
                AgentRow {
                    id: "b-id".into(),
                    name: "b".into(),
                    display_name: None,
                    handle: None,
                    badge: "green",
                    state: "connected, idle".into(),
                    call: Some("on a call".into()),
                    folder: PathBuf::from("bridges/b").display().to_string(),
                },
            ]
        );
        let json = serde_json::to_value(&window).unwrap();
        assert_eq!(json["agents"][0]["displayName"], "Coda");
        assert_eq!(json["global"]["startAtLogon"], true);
        assert_eq!(json["global"]["switchOffered"], false);
        assert_eq!(json["global"]["menuPeek"], true);
        assert_eq!(json["global"]["reduceMotion"], false);
        assert_eq!(json["global"]["configError"], "bad");
    }

    #[test]
    fn actions_are_enabled_by_state_in_order() {
        for (state, connected, expected) in enabled_cases() {
            let agent = agent_view(&view("a", state.clone(), connected), None, 0);
            assert_eq!(
                agent
                    .actions
                    .iter()
                    .map(|action| action.action)
                    .collect::<Vec<_>>(),
                [
                    "start",
                    "stop",
                    "restart_backend",
                    "reconnect",
                    "pause",
                    "resume"
                ]
            );
            let enabled: Vec<&str> = agent
                .actions
                .iter()
                .filter(|action| action.enabled)
                .map(|action| action.action)
                .collect();
            assert_eq!(enabled, expected, "{state:?}");
        }
    }

    #[test]
    fn the_call_shows_with_its_buttons() {
        let names = |actions: &[ActionView]| -> Vec<(&str, &str, bool)> {
            actions
                .iter()
                .map(|action| (action.action, action.label, action.enabled))
                .collect()
        };
        let mut view = view("a", AgentState::Working, true);
        let agent = agent_view(&view, None, 0);
        assert_eq!(agent.call, None);
        assert_eq!(
            names(&agent.call_actions),
            [("mute", "Mute agent", false), ("hangup", "Hang up", false)]
        );

        view.call = Some(call(CallPhase::Ringing, false));
        let agent = agent_view(&view, None, 0);
        assert_eq!(
            agent.call.as_deref(),
            Some("ringing (you are calling; confirm on your other device)")
        );
        assert_eq!(
            names(&agent.call_actions),
            [("mute", "Mute agent", false), ("hangup", "Hang up", true)]
        );

        view.call = Some(call(CallPhase::InCall, false));
        let agent = agent_view(&view, None, 0);
        assert_eq!(agent.call.as_deref(), Some("in a call — listening"));
        assert_eq!(
            names(&agent.call_actions),
            [("mute", "Mute agent", true), ("hangup", "Hang up", true)]
        );

        view.call = Some(call(CallPhase::InCall, true));
        view.refusal = Some(Refusal {
            action: AgentAction::Hangup,
            message: "no call is running (no_call)".into(),
            at: 5,
        });
        let agent = agent_view(&view, None, 0);
        assert_eq!(agent.call.as_deref(), Some("in a call — listening (muted)"));
        assert_eq!(
            names(&agent.call_actions),
            [
                ("unmute", "Unmute agent", true),
                ("hangup", "Hang up", true)
            ]
        );
        assert_eq!(
            agent.call_actions[1].refused.as_deref(),
            Some("no call is running (no_call)")
        );
        assert!(agent.actions.iter().all(|action| action.refused.is_none()));
        let json = serde_json::to_value(&agent).unwrap();
        assert_eq!(json["call"], "in a call — listening (muted)");
        assert_eq!(json["callActions"][0]["action"], "unmute");

        view.connected = false;
        let agent = agent_view(&view, None, 0);
        assert!(agent.call_actions.iter().all(|action| !action.enabled));
    }

    #[test]
    fn a_refusal_shows_on_its_action_only() {
        let mut view = view("a", AgentState::Idle, true);
        view.refusal = Some(Refusal {
            action: AgentAction::Pause,
            message: "a pause is running (busy)".into(),
            at: 5,
        });
        let agent = agent_view(&view, None, 0);
        for action in &agent.actions {
            if action.action == "pause" {
                assert_eq!(action.label, "Pause for upgrade");
                assert_eq!(action.refused.as_deref(), Some("a pause is running (busy)"));
            } else {
                assert_eq!(action.refused, None, "{}", action.action);
            }
        }
    }

    #[test]
    fn details_come_from_the_view_and_its_status() {
        let mut view = view("Coda (coda)", AgentState::Idle, true);
        for n in 0..60 {
            view.history.push(n, format!("state {n}"));
        }
        view.last_exit = Some(1);
        view.status = Some(status());
        let agent = agent_view(&view, None, 1_000 + 125_000);
        assert_eq!(agent.badge, "green");
        assert_eq!(agent.state, "connected, idle");
        assert_eq!(agent.session.as_deref(), Some("s-42"));
        assert_eq!(agent.coda_version.as_deref(), Some("1.2.3"));
        assert_eq!(agent.uptime.as_deref(), Some("2 min"));
        assert_eq!(agent.running_tasks, Some(2));
        assert_eq!(agent.last_error.as_deref(), Some("backend_crashed"));
        assert_eq!(agent.last_exit, Some(1));
        assert_eq!(agent.history.len(), 50);
        assert_eq!(
            agent.history[0],
            HistoryEntry {
                at: 59,
                state: "state 59".into()
            }
        );
        assert_eq!(agent.history[49].at, 10);

        view.last_error = Some("could not start the bridge".into());
        assert_eq!(
            agent_view(&view, None, 0).last_error.as_deref(),
            Some("could not start the bridge")
        );
        view.connected = false;
        assert_eq!(agent_view(&view, None, 200_000).uptime, None);
        view.connected = true;
        view.status.as_mut().unwrap().started_at = 0;
        assert_eq!(agent_view(&view, None, 200_000).uptime, None);
    }

    #[test]
    fn the_voice_adapter_shows_its_name_and_state() {
        let mut view = view("a", AgentState::Idle, true);
        view.status = Some(status());
        assert_eq!(
            agent_view(&view, None, 0).voice_adapter,
            None,
            "none configured"
        );
        let mut show = |adapter: AdapterStatus| {
            view.status.as_mut().unwrap().adapter = Some(adapter);
            agent_view(&view, None, 0).voice_adapter.unwrap()
        };
        let ready = AdapterStatus {
            state: "ready".into(),
            name: Some("whisper".into()),
            version: Some("1.2.3".into()),
            capabilities: Some(vec!["stt".into(), "tts".into()]),
            ..AdapterStatus::default()
        };
        assert_eq!(show(ready.clone()), "whisper 1.2.3 (ready)");
        assert_eq!(
            show(AdapterStatus {
                capabilities: Some(vec!["tts".into()]),
                ..ready
            }),
            "whisper 1.2.3 (ready, cannot listen)"
        );
        let state = |state: &str| AdapterStatus {
            state: state.into(),
            ..AdapterStatus::default()
        };
        assert_eq!(show(state("starting")), "starting");
        assert_eq!(show(state("handshaking")), "starting");
        assert_eq!(show(state("off")), "off");
        assert_eq!(
            show(AdapterStatus {
                reason: Some("not_found".into()),
                ..state("backoff")
            }),
            "backing off (not_found)"
        );
        let json = serde_json::to_value(agent_view(&view, None, 0)).unwrap();
        assert_eq!(json["voiceAdapter"], "backing off (not_found)");
    }

    #[test]
    fn a_view_without_status_has_no_details_from_the_bridge() {
        let agent = agent_view(&view("a", AgentState::Stopped, false), None, 5);
        assert_eq!(agent.session, None);
        assert_eq!(agent.coda_version, None);
        assert_eq!(agent.voice_adapter, None);
        assert_eq!(agent.uptime, None);
        assert_eq!(agent.running_tasks, None);
        assert_eq!(agent.last_error, None);
        assert!(agent.history.is_empty());
    }

    #[test]
    fn uptime_reads_in_two_units_at_most() {
        for (ms, text) in [
            (-5, "0 s"),
            (0, "0 s"),
            (45_999, "45 s"),
            (60_000, "1 min"),
            (12 * 60_000 + 59_000, "12 min"),
            (2 * 3_600_000, "2 h"),
            (2 * 3_600_000 + 5 * 60_000 + 30_000, "2 h 5 min"),
            (86_400_000, "1 d"),
            (3 * 86_400_000 + 4 * 3_600_000 + 59 * 60_000, "3 d 4 h"),
        ] {
            assert_eq!(uptime(ms), text, "{ms}");
        }
    }

    #[test]
    fn settings_come_from_tray_json_or_default() {
        let mut view = view("a", AgentState::Idle, true);
        view.autostart = false;
        let defaults = agent_view(&view, None, 0).settings;
        assert_eq!(
            defaults,
            AgentSettings {
                autostart: false,
                show_peek: true,
                peek_messages: 20,
                config_path: PathBuf::from("bridges/a")
                    .join("config.json")
                    .display()
                    .to_string(),
            }
        );
        let instance = tray_config::Instance {
            autostart: true,
            show_peek: false,
            peek_messages: 99,
            ..tray_config::Instance::new(view.dir.clone())
        };
        let settings = agent_view(&view, Some(&instance), 0).settings;
        assert!(settings.autostart);
        assert!(!settings.show_peek);
        assert_eq!(settings.peek_messages, 50);
    }

    #[test]
    fn the_agent_view_is_camel_case_json() {
        let mut view = view("a", AgentState::Idle, true);
        view.status = Some(status());
        view.last_exit = Some(3);
        let json = serde_json::to_value(agent_view(&view, None, 61_000)).unwrap();
        for key in [
            "codaVersion",
            "runningTasks",
            "lastError",
            "lastExit",
            "uptime",
            "history",
        ] {
            assert!(!json[key].is_null(), "{key}");
        }
        assert_eq!(json["settings"]["showPeek"], true);
        assert_eq!(json["settings"]["peekMessages"], 20);
        assert!(json["settings"]["configPath"].is_string());
        assert_eq!(json["actions"][0]["action"], "start");
        assert_eq!(json["actions"][0]["refused"], serde_json::Value::Null);
    }

    #[test]
    fn notices_read_as_sentences() {
        let gave_up = Notice::GaveUp {
            dir: PathBuf::from("a"),
            name: "Coda (coda)".into(),
            code: Some(1),
        };
        assert_eq!(
            notice_text(&gave_up),
            "Coda (coda) stopped after 5 failures (exit 1). Open its log from the tray window."
        );
        let killed = Notice::GaveUp {
            dir: PathBuf::from("a"),
            name: "a".into(),
            code: None,
        };
        assert_eq!(
            notice_text(&killed),
            "a stopped after 5 failures. Open its log from the tray window."
        );
        let not_set_up = Notice::NotSetUp {
            dir: PathBuf::from("a"),
            name: "a".into(),
        };
        assert_eq!(
            notice_text(&not_set_up),
            "a is not set up: run keygen and enroll."
        );
    }
}
