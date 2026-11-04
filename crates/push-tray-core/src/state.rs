//! What the tray shows for an agent: its state, the icon state, a label and the tooltip.

use std::time::Duration;

use push_bridge_control::{BridgeState, CallDirection, CallInfo, CallPhase};

use crate::policy::MAX_EXITS;

/// An agent (bridge instance) as the tray sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    /// Connected, nothing to do.
    Idle,
    /// A turn or a slot command runs.
    Working,
    /// A card waits for the owner; the detail names its kind ("approval", …).
    Waiting(Option<String>),
    /// The process is up, signing in or opening the conversation.
    Starting,
    /// The messenger connection is down; the bridge reconnects.
    Reconnecting,
    /// Coda is down and restarts.
    Restarting,
    /// Upgrade mode.
    Paused,
    Stopping,
    /// Not running: stopped by the owner, or never started.
    Stopped,
    /// The bridge exited; `retry_in` when the tray starts it again.
    Failed {
        code: Option<i32>,
        retry_in: Option<Duration>,
    },
    /// The bridge failed too often; the tray no longer restarts it.
    GaveUp {
        code: Option<i32>,
    },
    /// The bridge exited with code 2: no key or not enrolled.
    NotSetUp,
    /// Reachable, but not started by the tray: by the instance's Scheduled Task (`task`),
    /// or from outside the tray (a console, a script).
    External {
        task: bool,
    },
}

/// The tray icon's state, from the least to the most urgent (`Ord` follows this order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IconState {
    /// Connected, idle.
    Normal,
    /// Working.
    Busy,
    /// On the way between states: starting, stopping, Coda restarting, reconnecting.
    Transition,
    /// Paused for an upgrade.
    Paused,
    /// Waiting for the owner.
    Waiting,
    /// Up, but the tray cannot reach it.
    Disconnected,
    /// Stopped, failed, gave up or not set up.
    Error,
}

impl IconState {
    /// Every icon state, from the least to the most urgent.
    pub const ALL: [IconState; 7] = [
        IconState::Normal,
        IconState::Busy,
        IconState::Transition,
        IconState::Paused,
        IconState::Waiting,
        IconState::Disconnected,
        IconState::Error,
    ];
}

/// The icon an agent's state asks for.
pub fn icon_for(state: &AgentState) -> IconState {
    match state {
        AgentState::Idle | AgentState::External { .. } => IconState::Normal,
        AgentState::Working => IconState::Busy,
        AgentState::Waiting(_) => IconState::Waiting,
        AgentState::Paused => IconState::Paused,
        AgentState::Starting
        | AgentState::Reconnecting
        | AgentState::Restarting
        | AgentState::Stopping => IconState::Transition,
        AgentState::Stopped
        | AgentState::Failed { .. }
        | AgentState::GaveUp { .. }
        | AgentState::NotSetUp => IconState::Error,
    }
}

/// The icon for an agent in `state` whose control channel is `connected` or not: an agent
/// that is up but unreachable is [`IconState::Disconnected`].
pub fn icon_for_view(state: &AgentState, connected: bool) -> IconState {
    let up = matches!(
        state,
        AgentState::Idle
            | AgentState::Working
            | AgentState::Waiting(_)
            | AgentState::Paused
            | AgentState::External { .. }
    );
    if up && !connected {
        IconState::Disconnected
    } else {
        icon_for(state)
    }
}

/// The most urgent of `states`; `None` when there are none (no agents, no badge).
pub fn worst(states: impl IntoIterator<Item = IconState>) -> Option<IconState> {
    states.into_iter().max()
}

/// The state for people, e.g. "waiting for you (approval)".
pub fn label(state: &AgentState) -> String {
    match state {
        AgentState::Idle => "connected, idle".to_owned(),
        AgentState::Working => "working".to_owned(),
        AgentState::Waiting(Some(detail)) if !detail.is_empty() => {
            format!("waiting for you ({detail})")
        }
        AgentState::Waiting(_) => "waiting for you".to_owned(),
        AgentState::Starting => "starting".to_owned(),
        AgentState::Reconnecting => "disconnected, reconnecting".to_owned(),
        AgentState::Restarting => "Coda restarting".to_owned(),
        AgentState::Paused => "paused for an upgrade".to_owned(),
        AgentState::Stopping => "stopping".to_owned(),
        AgentState::Stopped => "stopped".to_owned(),
        AgentState::Failed { code, retry_in } => {
            let mut text = format!("failed{}", exit_suffix(*code));
            if let Some(delay) = retry_in {
                text.push_str(&format!(", retrying in {} s", whole_seconds(*delay)));
            }
            text
        }
        AgentState::GaveUp { code } => {
            format!("stopped after {MAX_EXITS} failures{}", exit_suffix(*code))
        }
        AgentState::NotSetUp => "Not set up: run keygen and enroll".to_owned(),
        AgentState::External { task: true } => "running (Scheduled Task)".to_owned(),
        AgentState::External { task: false } => "running (started outside the tray)".to_owned(),
    }
}

fn exit_suffix(code: Option<i32>) -> String {
    code.map(|code| format!(" (exit {code})"))
        .unwrap_or_default()
}

/// Seconds, rounded up: "retrying in 1 s" rather than "in 0 s".
fn whole_seconds(delay: Duration) -> u128 {
    delay.as_millis().div_ceil(1000)
}

/// The voice call for people, as the window's Overview shows it: "ringing (you are
/// calling; confirm on your other device)" (the owner's call waits for the owner's
/// confirmation), "calling you" (the agent rings the owner), "connecting", "in a call —
/// speaking" / "— listening"; " (muted)" while the agent's voice is muted.
pub fn call_label(call: &CallInfo) -> String {
    let mut text = match (call.phase, call.direction) {
        (CallPhase::Ringing, CallDirection::Incoming) => {
            "ringing (you are calling; confirm on your other device)".to_owned()
        }
        (CallPhase::Ringing, CallDirection::Outgoing) => "calling you".to_owned(),
        (CallPhase::Connecting, _) => "connecting".to_owned(),
        (CallPhase::InCall, _) => "in a call".to_owned(),
    };
    if call.phase == CallPhase::InCall {
        if call.speaking {
            text.push_str(" — speaking");
        } else if call.listening {
            text.push_str(" — listening");
        }
    }
    if call.muted && call.phase != CallPhase::Ringing {
        text.push_str(" (muted)");
    }
    text
}

/// The voice call in a few words, for the menu's agent line and the agent list: "calling
/// you", "your call is ringing", "on a call" (also while connecting).
pub fn call_short(call: &CallInfo) -> &'static str {
    match (call.phase, call.direction) {
        (CallPhase::Ringing, CallDirection::Outgoing) => "calling you",
        (CallPhase::Ringing, CallDirection::Incoming) => "your call is ringing",
        (CallPhase::Connecting | CallPhase::InCall, _) => "on a call",
    }
}

/// The agent state for a state the bridge reports.
pub fn from_bridge(state: BridgeState, detail: Option<String>) -> AgentState {
    match state {
        BridgeState::Starting => AgentState::Starting,
        BridgeState::Idle => AgentState::Idle,
        BridgeState::Working => AgentState::Working,
        BridgeState::Waiting => AgentState::Waiting(detail),
        BridgeState::Reconnecting => AgentState::Reconnecting,
        BridgeState::Restarting => AgentState::Restarting,
        BridgeState::Paused => AgentState::Paused,
        BridgeState::Stopping => AgentState::Stopping,
    }
}

/// The longest tooltip, in UTF-16 units (Windows' `NOTIFYICONDATA.szTip` holds 128 with
/// the terminating NUL).
pub const TOOLTIP_MAX: usize = 127;

/// The tray icon's tooltip: a line per agent, "Coda on Devbox: waiting for you
/// (approval)"; "push-agent-tray" and "No agents" without agents. At most
/// [`TOOLTIP_MAX`] UTF-16 units, cut with "…".
pub fn tooltip(agents: &[(String, AgentState)]) -> String {
    let text = if agents.is_empty() {
        "push-agent-tray\nNo agents".to_owned()
    } else {
        agents
            .iter()
            .map(|(name, state)| format!("{name}: {}", label(state)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    cap_utf16(text, TOOLTIP_MAX)
}

fn cap_utf16(text: String, max: usize) -> String {
    if text.encode_utf16().count() <= max {
        return text;
    }
    let budget = max.saturating_sub(1);
    let mut used = 0;
    let mut cut = String::new();
    for ch in text.chars() {
        used += ch.len_utf16();
        if used > budget {
            break;
        }
        cut.push(ch);
    }
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(code: Option<i32>, retry_secs: Option<u64>) -> AgentState {
        AgentState::Failed {
            code,
            retry_in: retry_secs.map(Duration::from_secs),
        }
    }

    #[test]
    fn every_state_has_its_icon() {
        let cases = [
            (AgentState::Idle, IconState::Normal),
            (AgentState::External { task: true }, IconState::Normal),
            (AgentState::External { task: false }, IconState::Normal),
            (AgentState::Working, IconState::Busy),
            (AgentState::Waiting(None), IconState::Waiting),
            (AgentState::Waiting(Some("plan".into())), IconState::Waiting),
            (AgentState::Paused, IconState::Paused),
            (AgentState::Starting, IconState::Transition),
            (AgentState::Reconnecting, IconState::Transition),
            (AgentState::Restarting, IconState::Transition),
            (AgentState::Stopping, IconState::Transition),
            (AgentState::Stopped, IconState::Error),
            (failed(Some(1), Some(4)), IconState::Error),
            (AgentState::GaveUp { code: Some(1) }, IconState::Error),
            (AgentState::NotSetUp, IconState::Error),
        ];
        for (state, icon) in cases {
            assert_eq!(icon_for(&state), icon, "{state:?}");
            assert_eq!(icon_for_view(&state, true), icon, "{state:?}");
        }
    }

    #[test]
    fn an_agent_that_is_up_but_not_connected_is_disconnected() {
        let cases = [
            (AgentState::Idle, IconState::Disconnected),
            (AgentState::Working, IconState::Disconnected),
            (AgentState::Waiting(None), IconState::Disconnected),
            (AgentState::Paused, IconState::Disconnected),
            (AgentState::External { task: true }, IconState::Disconnected),
            (AgentState::Starting, IconState::Transition),
            (AgentState::Reconnecting, IconState::Transition),
            (AgentState::Restarting, IconState::Transition),
            (AgentState::Stopping, IconState::Transition),
            (AgentState::Stopped, IconState::Error),
            (failed(Some(1), None), IconState::Error),
            (AgentState::GaveUp { code: None }, IconState::Error),
            (AgentState::NotSetUp, IconState::Error),
        ];
        for (state, icon) in cases {
            assert_eq!(icon_for_view(&state, false), icon, "{state:?}");
        }
    }

    #[test]
    fn severity_runs_from_normal_to_error() {
        let mut sorted = IconState::ALL;
        sorted.sort();
        assert_eq!(sorted, IconState::ALL);
        assert_eq!(
            IconState::ALL,
            [
                IconState::Normal,
                IconState::Busy,
                IconState::Transition,
                IconState::Paused,
                IconState::Waiting,
                IconState::Disconnected,
                IconState::Error,
            ]
        );
    }

    #[test]
    fn the_worst_icon_wins() {
        assert_eq!(worst([]), None);
        assert_eq!(worst([IconState::Busy]), Some(IconState::Busy));
        assert_eq!(
            worst([IconState::Normal, IconState::Waiting, IconState::Busy]),
            Some(IconState::Waiting)
        );
        assert_eq!(
            worst([IconState::Waiting, IconState::Paused]),
            Some(IconState::Waiting)
        );
        assert_eq!(
            worst([IconState::Error, IconState::Disconnected, IconState::Normal]),
            Some(IconState::Error)
        );
        assert_eq!(
            worst([IconState::Paused, IconState::Busy, IconState::Normal]),
            Some(IconState::Paused)
        );
        assert_eq!(
            worst([IconState::Busy, IconState::Transition]),
            Some(IconState::Transition)
        );
    }

    #[test]
    fn labels_read_as_documented() {
        let cases = [
            (AgentState::Idle, "connected, idle"),
            (AgentState::Working, "working"),
            (
                AgentState::Waiting(Some("approval".into())),
                "waiting for you (approval)",
            ),
            (AgentState::Waiting(None), "waiting for you"),
            (AgentState::Paused, "paused for an upgrade"),
            (AgentState::Reconnecting, "disconnected, reconnecting"),
            (AgentState::Restarting, "Coda restarting"),
            (AgentState::Stopped, "stopped"),
            (failed(Some(1), Some(4)), "failed (exit 1), retrying in 4 s"),
            (failed(Some(1), None), "failed (exit 1)"),
            (failed(None, None), "failed"),
            (
                AgentState::GaveUp { code: Some(1) },
                "stopped after 5 failures (exit 1)",
            ),
            (AgentState::NotSetUp, "Not set up: run keygen and enroll"),
            (AgentState::Starting, "starting"),
            (AgentState::Stopping, "stopping"),
            (
                AgentState::External { task: true },
                "running (Scheduled Task)",
            ),
            (
                AgentState::External { task: false },
                "running (started outside the tray)",
            ),
        ];
        for (state, text) in cases {
            assert_eq!(label(&state), text, "{state:?}");
        }
        let soon = AgentState::Failed {
            code: Some(1),
            retry_in: Some(Duration::from_millis(1500)),
        };
        assert_eq!(label(&soon), "failed (exit 1), retrying in 2 s");
    }

    fn call(phase: CallPhase, direction: CallDirection) -> CallInfo {
        CallInfo {
            phase,
            direction,
            speaking: false,
            listening: false,
            muted: false,
            since: 1,
        }
    }

    #[test]
    fn calls_read_by_phase_direction_and_flags() {
        use CallDirection::{Incoming, Outgoing};
        use CallPhase::{Connecting, InCall, Ringing};
        let cases = [
            (
                call(Ringing, Incoming),
                "ringing (you are calling; confirm on your other device)",
                "your call is ringing",
            ),
            (call(Ringing, Outgoing), "calling you", "calling you"),
            (call(Connecting, Incoming), "connecting", "on a call"),
            (call(Connecting, Outgoing), "connecting", "on a call"),
            (call(InCall, Incoming), "in a call", "on a call"),
            (
                CallInfo {
                    speaking: true,
                    listening: true,
                    ..call(InCall, Outgoing)
                },
                "in a call — speaking",
                "on a call",
            ),
            (
                CallInfo {
                    listening: true,
                    muted: true,
                    ..call(InCall, Incoming)
                },
                "in a call — listening (muted)",
                "on a call",
            ),
            (
                CallInfo {
                    muted: true,
                    ..call(Connecting, Incoming)
                },
                "connecting (muted)",
                "on a call",
            ),
            // Flags of a ringing call mean nothing yet.
            (
                CallInfo {
                    speaking: true,
                    muted: true,
                    ..call(Ringing, Outgoing)
                },
                "calling you",
                "calling you",
            ),
        ];
        for (call, text, short) in cases {
            assert_eq!(call_label(&call), text, "{call:?}");
            assert_eq!(call_short(&call), short, "{call:?}");
        }
    }

    #[test]
    fn bridge_states_map_one_to_one() {
        let cases = [
            (BridgeState::Starting, AgentState::Starting),
            (BridgeState::Idle, AgentState::Idle),
            (BridgeState::Working, AgentState::Working),
            (
                BridgeState::Waiting,
                AgentState::Waiting(Some("detail".into())),
            ),
            (BridgeState::Reconnecting, AgentState::Reconnecting),
            (BridgeState::Restarting, AgentState::Restarting),
            (BridgeState::Paused, AgentState::Paused),
            (BridgeState::Stopping, AgentState::Stopping),
        ];
        for (bridge, agent) in cases {
            assert_eq!(from_bridge(bridge, Some("detail".into())), agent);
        }
        assert_eq!(
            from_bridge(BridgeState::Waiting, None),
            AgentState::Waiting(None)
        );
    }

    #[test]
    fn the_tooltip_has_a_line_per_agent() {
        assert_eq!(tooltip(&[]), "push-agent-tray\nNo agents");
        let agents = [
            (
                "Coda on Devbox".to_owned(),
                AgentState::Waiting(Some("approval".into())),
            ),
            ("Coda at home".to_owned(), AgentState::Idle),
        ];
        assert_eq!(
            tooltip(&agents),
            "Coda on Devbox: waiting for you (approval)\nCoda at home: connected, idle"
        );
    }

    #[test]
    fn a_long_tooltip_is_cut_at_127_utf16_units() {
        let agents: Vec<_> = (0..10)
            .map(|n| (format!("Agent {n} 🙂"), AgentState::Working))
            .collect();
        let text = tooltip(&agents);
        let units = text.encode_utf16().count();
        // A surrogate pair is never split, so the cut may be one unit short.
        assert!((TOOLTIP_MAX - 1..=TOOLTIP_MAX).contains(&units), "{units}");
        assert!(text.ends_with('…'));
        assert!(text.starts_with("Agent 0 🙂: working\n"));
        let exact = vec![("x".repeat(TOOLTIP_MAX - 9), AgentState::Working)];
        assert_eq!(tooltip(&exact).encode_utf16().count(), TOOLTIP_MAX);
        assert!(!tooltip(&exact).ends_with('…'));
    }
}
