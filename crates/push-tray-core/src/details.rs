//! What the shell shows outside the menu: an agent's details window and the notifications.
//! Content-free like everything else: states, names, counts and error codes only.

use serde::Serialize;

use crate::policy::MAX_EXITS;
use crate::state::label;
use crate::supervisor::{InstanceView, Notice};

/// A details window's label starts with this, followed by the instance id.
pub const WINDOW_PREFIX: &str = "details-";

/// The details window's label for the instance `id`.
pub fn window_label(id: &str) -> String {
    format!("{WINDOW_PREFIX}{id}")
}

/// The instance id a details window's label names.
pub fn instance_of_window(label: &str) -> Option<&str> {
    label
        .strip_prefix(WINDOW_PREFIX)
        .filter(|id| !id.is_empty())
}

/// The details window's title.
pub fn window_title(name: &str) -> String {
    format!("{name} — details")
}

/// An agent's details, as the details window gets them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailsView {
    pub name: String,
    pub state: String,
    pub folder: String,
    pub session: Option<String>,
    pub coda_version: Option<String>,
    pub running_tasks: Option<usize>,
    pub last_error: Option<String>,
    pub last_exit: Option<i32>,
    /// Newest first.
    pub history: Vec<HistoryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryEntry {
    /// Unix milliseconds.
    pub at: i64,
    pub state: String,
}

/// The details of `view`. The tray's own error wins over the bridge's last error code.
pub fn details_view(view: &InstanceView) -> DetailsView {
    let status = view.status.as_ref();
    let backend = status.and_then(|status| status.backend.as_ref());
    DetailsView {
        name: view.name.clone(),
        state: label(&view.state),
        folder: view.dir.display().to_string(),
        session: status.and_then(|status| status.session.clone()),
        coda_version: backend.and_then(|backend| backend.version.clone()),
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
    }
}

/// The notification for `notice`.
pub fn notice_text(notice: &Notice) -> String {
    match notice {
        Notice::GaveUp { name, code, .. } => {
            let exit = code.map_or_else(String::new, |code| format!(" (exit {code})"));
            format!(
                "{name} stopped after {MAX_EXITS} failures{exit}. Open its log from the tray menu."
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
    use crate::history::History;
    use crate::state::AgentState;
    use push_bridge_control::{BackendInfo, LastError, Status};
    use std::path::PathBuf;

    fn view() -> InstanceView {
        InstanceView {
            dir: PathBuf::from("bridges/a"),
            id: "0011".into(),
            name: "Coda (coda)".into(),
            state: AgentState::Idle,
            started_by_tray: true,
            connected: true,
            autostart: true,
            task: None,
            status: None,
            history: History::new(),
            last_exit: None,
            last_error: None,
        }
    }

    #[test]
    fn window_labels_name_the_instance() {
        assert_eq!(window_label("0011"), "details-0011");
        assert_eq!(instance_of_window("details-0011"), Some("0011"));
        assert_eq!(instance_of_window("details-"), None);
        assert_eq!(instance_of_window("main"), None);
        assert_eq!(window_title("Coda (coda)"), "Coda (coda) — details");
    }

    #[test]
    fn details_come_from_the_view_and_its_status() {
        let mut view = view();
        view.history.push(1, "starting");
        view.history.push(2, "connected, idle");
        view.last_exit = Some(1);
        view.status = Some(Status {
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
            ..Status::default()
        });
        let details = details_view(&view);
        assert_eq!(details.state, "connected, idle");
        assert_eq!(
            details.folder,
            PathBuf::from("bridges/a").display().to_string()
        );
        assert_eq!(details.session.as_deref(), Some("s-42"));
        assert_eq!(details.coda_version.as_deref(), Some("1.2.3"));
        assert_eq!(details.running_tasks, Some(2));
        assert_eq!(details.last_error.as_deref(), Some("backend_crashed"));
        assert_eq!(details.last_exit, Some(1));
        assert_eq!(
            details.history,
            [
                HistoryEntry {
                    at: 2,
                    state: "connected, idle".into()
                },
                HistoryEntry {
                    at: 1,
                    state: "starting".into()
                },
            ]
        );
        let json = serde_json::to_value(&details).unwrap();
        assert_eq!(json["codaVersion"], "1.2.3");
        assert_eq!(json["runningTasks"], 2);
        assert_eq!(json["lastExit"], 1);

        view.last_error = Some("could not start the bridge".into());
        assert_eq!(
            details_view(&view).last_error.as_deref(),
            Some("could not start the bridge")
        );
    }

    #[test]
    fn a_view_without_status_has_no_details_from_the_bridge() {
        let details = details_view(&view());
        assert_eq!(details.session, None);
        assert_eq!(details.coda_version, None);
        assert_eq!(details.running_tasks, None);
        assert_eq!(details.last_error, None);
        assert!(details.history.is_empty());
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
            "Coda (coda) stopped after 5 failures (exit 1). Open its log from the tray menu."
        );
        let killed = Notice::GaveUp {
            dir: PathBuf::from("a"),
            name: "a".into(),
            code: None,
        };
        assert_eq!(
            notice_text(&killed),
            "a stopped after 5 failures. Open its log from the tray menu."
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
