//! The commands the tray window's page calls (`window.__TAURI_INTERNALS__.invoke`). Each
//! one answers only the window labelled [`window::LABEL`]. All are async, so none runs on
//! the main thread. Errors are short texts the page shows inline.

use std::path::{Path, PathBuf};
use std::time::Duration;

use push_tray_core::logtail::{self, Chunk, TAIL_LINES};
use push_tray_core::menu::{self, AgentAction};
use push_tray_core::open::open_path;
use push_tray_core::peek::{self, ChatEntry};
use push_tray_core::schedtask::{self, Offer};
use push_tray_core::state::label;
use push_tray_core::supervisor::{Command, InstanceView};
use push_tray_core::tray_config::{
    PEEK_MESSAGES_DEFAULT, PEEK_MESSAGES_MAX, PEEK_MESSAGES_MIN, clamp_peek_messages,
};
use push_tray_core::window::{self as view_model, AgentView, GlobalSettings, WindowView};
use serde::Serialize;
use serde_json::Value;
use tauri::ipc::Invoke;
use tauri::{AppHandle, Manager, WebviewWindow, Wry};

use crate::app::{self, Shell};
use crate::window;

/// How long `peek` waits for a `peek_changed` before fetching anyway.
const PEEK_WAIT: Duration = Duration::from_secs(2);
const GONE: &str = "This agent is no longer in the tray.";
const HIDDEN: &str = "hidden";

pub fn handler() -> impl Fn(Invoke<Wry>) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        window_view,
        agent_view,
        act,
        open_folder,
        open_config,
        open_log,
        log_tail,
        peek,
        open_messenger,
        set_agent_setting,
        set_global_setting,
        remove_agent,
        add_folder,
        set_start_at_logon,
        switch_over,
        quit,
        report_motion,
    ]
}

fn main_only(window: &WebviewWindow) -> Result<(), String> {
    if window.label() == window::LABEL {
        Ok(())
    } else {
        Err("not the tray window".to_owned())
    }
}

fn agent(app: &AppHandle, id: &str) -> Result<InstanceView, String> {
    app.state::<Shell>().view(id).ok_or_else(|| GONE.to_owned())
}

/// The agent list and the tray's settings, with the agent the page should select (asked
/// for by the menu; handed out once).
#[derive(Serialize)]
pub struct WindowReply {
    view: WindowView,
    select: Option<String>,
}

#[tauri::command]
async fn window_view(window: WebviewWindow, app: AppHandle) -> Result<WindowReply, String> {
    main_only(&window)?;
    let shell = app.state::<Shell>();
    let views = shell.views.borrow().clone();
    let (global, select) = {
        let mut state = shell.state();
        let global = GlobalSettings {
            start_at_logon: state.tray_autostart,
            switch_offered: !state.switching
                && matches!(schedtask::offer(&state.found), Offer::SwitchOver(_)),
            menu_peek: state.config.menu_peek,
            reduce_motion: state.config.reduce_motion,
            config_error: state.config_error.clone(),
        };
        (global, state.selected.take())
    };
    Ok(WindowReply {
        view: view_model::window_view(&views, global),
        select,
    })
}

#[tauri::command]
async fn agent_view(
    window: WebviewWindow,
    app: AppHandle,
    id: String,
) -> Result<AgentView, String> {
    main_only(&window)?;
    let view = agent(&app, &id)?;
    let instance = app.state::<Shell>().state().config.find(&view.dir).cloned();
    Ok(view_model::agent_view(
        &view,
        instance.as_ref(),
        push_bridge_control::now_ms(),
    ))
}

/// Start, Stop, Restart <backend>, Reconnect, Pause for upgrade, Resume, Mute agent, Unmute
/// agent or Hang up. The bridge's later refusal shows in the agent's view.
#[tauri::command]
async fn act(
    window: WebviewWindow,
    app: AppHandle,
    id: String,
    action: String,
) -> Result<(), String> {
    main_only(&window)?;
    let action = AgentAction::parse(&action).ok_or_else(|| "Unknown action.".to_owned())?;
    let view = agent(&app, &id)?;
    if !menu::enabled(&view, action) {
        let call_action = matches!(
            action,
            AgentAction::Mute | AgentAction::Unmute | AgentAction::Hangup
        );
        if call_action && view.connected && view.call.is_none() {
            return Err("No call is running.".to_owned());
        }
        return Err(format!("Not available while {}.", label(&view.state)));
    }
    let dir = view.dir;
    app.state::<Shell>().send(match action {
        AgentAction::Start => Command::Start(dir),
        AgentAction::Stop => Command::Stop(dir),
        AgentAction::RestartBackend => Command::RestartBackend(dir),
        AgentAction::Reconnect => Command::Reconnect(dir),
        AgentAction::Pause => Command::Pause(dir),
        AgentAction::Resume => Command::Resume(dir),
        AgentAction::Mute => Command::Mute(dir),
        AgentAction::Unmute => Command::Unmute(dir),
        AgentAction::Hangup => Command::Hangup(dir),
    })
}

fn open(path: &Path) -> Result<(), String> {
    open_path(path).map_err(|error| format!("Could not open {}: {error}", path.display()))
}

#[tauri::command]
async fn open_folder(window: WebviewWindow, app: AppHandle, id: String) -> Result<(), String> {
    main_only(&window)?;
    open(&agent(&app, &id)?.dir)
}

/// The bridge's config.json, in the default editor (the tray never writes it).
#[tauri::command]
async fn open_config(window: WebviewWindow, app: AppHandle, id: String) -> Result<(), String> {
    main_only(&window)?;
    let path = agent(&app, &id)?.dir.join("config.json");
    if !path.is_file() {
        return Err(format!("No config.json in {}.", path.display()));
    }
    open(&path)
}

#[tauri::command]
async fn open_log(window: WebviewWindow, app: AppHandle, id: String) -> Result<(), String> {
    main_only(&window)?;
    let path = menu::log_path(&agent(&app, &id)?.dir);
    if !path.is_file() {
        return Err("No log yet".to_owned());
    }
    open(&path)
}

/// The log's last lines (`offset` null), else the complete lines appended after `offset`.
#[tauri::command]
async fn log_tail(
    window: WebviewWindow,
    app: AppHandle,
    id: String,
    offset: Option<u64>,
) -> Result<Chunk, String> {
    main_only(&window)?;
    let path = menu::log_path(&agent(&app, &id)?.dir);
    tauri::async_runtime::spawn_blocking(move || match offset {
        None => logtail::read_tail(&path, TAIL_LINES),
        Some(offset) => logtail::read_from(&path, offset, TAIL_LINES),
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| format!("The log could not be read: {error}"))
}

/// The Chat tab.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeekView {
    entries: Vec<ChatEntry>,
    /// The agent's `peek_changes` these entries reflect (the next call's `seen`).
    seen: u64,
    /// The bridge has the peek turned off (`"peek": false`).
    disabled: bool,
    /// The tray's "Show chat peek" is off for this agent.
    off: bool,
    messenger_url: Option<String>,
    error: Option<String>,
}

fn messenger_url(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("config.json")).ok()?;
    peek::messenger_url(&text)
}

/// The agent's last messages. With `seen` equal to the agent's current `peek_changes`,
/// first waits up to [`PEEK_WAIT`] for a change. Refused (`hidden`) while the window is
/// not visible.
#[tauri::command]
async fn peek(
    window: WebviewWindow,
    app: AppHandle,
    id: String,
    seen: Option<u64>,
) -> Result<PeekView, String> {
    main_only(&window)?;
    let visible = |window: &WebviewWindow| {
        window.is_visible().unwrap_or(false) && !window.is_minimized().unwrap_or(true)
    };
    if !visible(&window) {
        return Err(HIDDEN.to_owned());
    }
    let shell = app.state::<Shell>();
    let mut view = agent(&app, &id)?;
    let (show_peek, limit) = shell
        .state()
        .config
        .find(&view.dir)
        .map_or((true, PEEK_MESSAGES_DEFAULT), |instance| {
            (instance.show_peek, instance.peek_messages())
        });
    let mut reply = PeekView {
        entries: Vec::new(),
        seen: view.peek_changes,
        disabled: false,
        off: !show_peek,
        messenger_url: messenger_url(&view.dir),
        error: None,
    };
    if !show_peek {
        return Ok(reply);
    }
    if seen == Some(view.peek_changes) {
        let mut views = shell.views.clone();
        let (changes, control) = (view.peek_changes, view.control.clone());
        let wanted = id.clone();
        let changed = async move {
            let _ = views
                .wait_for(|views| {
                    views
                        .iter()
                        .find(|view| view.id == wanted)
                        .is_none_or(|view| view.peek_changes != changes || view.control != control)
                })
                .await
                .map(|_| ());
        };
        let _ = tokio::time::timeout(PEEK_WAIT, changed).await;
        view = agent(&app, &id)?;
        reply.seen = view.peek_changes;
    }
    let Some(control) = view.control else {
        reply.error = Some("not connected".to_owned());
        return Ok(reply);
    };
    // The window may have been hidden or minimised while waiting.
    if !visible(&window) {
        return Err(HIDDEN.to_owned());
    }
    match control.peek(limit as usize).await {
        Ok(entries) => reply.entries = peek::chat_entries(entries),
        Err(error) if error.code == "peek_disabled" => reply.disabled = true,
        Err(error) => reply.error = Some(format!("{} ({})", error.message, error.code)),
    }
    Ok(reply)
}

/// Opens the messenger's web address (recomputed from config.json, never the page's).
#[tauri::command]
async fn open_messenger(window: WebviewWindow, app: AppHandle, id: String) -> Result<(), String> {
    main_only(&window)?;
    let url = messenger_url(&agent(&app, &id)?.dir)
        .filter(|url| url.starts_with("https://"))
        .ok_or_else(|| "config.json has no https messenger address.".to_owned())?;
    open_url(&url).map_err(|error| format!("Could not open the messenger: {error}"))
}

/// Opens an https URL in the default browser: one argument, no shell.
fn open_url(url: &str) -> std::io::Result<()> {
    let mut child = browser()
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(windows)]
fn browser() -> std::process::Command {
    let root = std::env::var_os("SystemRoot")
        .filter(|root| !root.is_empty())
        .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
    std::process::Command::new(root.join("explorer.exe"))
}

#[cfg(not(windows))]
fn browser() -> std::process::Command {
    std::process::Command::new("xdg-open")
}

fn flag(value: &Value) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| "Expected true or false.".to_owned())
}

fn changed(edited: bool) -> Result<(), String> {
    if edited {
        Ok(())
    } else {
        Err("This agent is not in tray.json.".to_owned())
    }
}

/// `autostart` (bool), `show_peek` (bool) or `peek_messages` (10–50, clamped), saved in
/// tray.json.
#[tauri::command]
async fn set_agent_setting(
    window: WebviewWindow,
    app: AppHandle,
    id: String,
    key: String,
    value: Value,
) -> Result<(), String> {
    main_only(&window)?;
    let dir = agent(&app, &id)?.dir;
    let shell = app.state::<Shell>();
    match key.as_str() {
        "autostart" => {
            let on = flag(&value)?;
            changed(shell.edit_config(|config| Ok(config.set_autostart(&dir, on)))?)?;
            shell.set_instances();
        }
        "show_peek" => {
            let on = flag(&value)?;
            changed(shell.edit_config(|config| Ok(config.set_show_peek(&dir, on)))?)?;
            shell.peek_wake.notify_one();
        }
        "peek_messages" => {
            let messages = value
                .as_f64()
                .ok_or_else(|| "Expected a number.".to_owned())?
                .round()
                .clamp(f64::from(PEEK_MESSAGES_MIN), f64::from(PEEK_MESSAGES_MAX))
                as u32;
            let messages = clamp_peek_messages(messages);
            changed(shell.edit_config(|config| Ok(config.set_peek_messages(&dir, messages)))?)?;
        }
        _ => return Err("Unknown setting.".to_owned()),
    }
    Ok(())
}

/// `menu_peek` or `reduce_motion` (bool), saved in tray.json.
#[tauri::command]
async fn set_global_setting(
    window: WebviewWindow,
    app: AppHandle,
    key: String,
    value: Value,
) -> Result<(), String> {
    main_only(&window)?;
    let on = flag(&value)?;
    let shell = app.state::<Shell>();
    match key.as_str() {
        "menu_peek" => shell.edit_config(|config| {
            config.menu_peek = on;
            Ok(true)
        })?,
        "reduce_motion" => shell.edit_config(|config| {
            config.reduce_motion = on;
            Ok(true)
        })?,
        _ => return Err("Unknown setting.".to_owned()),
    };
    shell.peek_wake.notify_one();
    shell.request_refresh();
    Ok(())
}

/// Removes the agent from tray.json (it stays enrolled and its folder is untouched); the
/// supervisor stops it when the tray runs it.
#[tauri::command]
async fn remove_agent(window: WebviewWindow, app: AppHandle, id: String) -> Result<(), String> {
    main_only(&window)?;
    let dir = agent(&app, &id)?.dir;
    changed(
        app.state::<Shell>()
            .edit_config(|config| Ok(config.remove(&dir)))?,
    )?;
    app::rescan(&app).await;
    app.state::<Shell>().set_instances();
    Ok(())
}

/// Picks a bridge folder and adds it; the new agent's id (none when cancelled).
#[tauri::command]
async fn add_folder(window: WebviewWindow, app: AppHandle) -> Result<Option<String>, String> {
    main_only(&window)?;
    if let Some(refusal) = app.state::<Shell>().refusal() {
        return Err(refusal);
    }
    match app::pick_folder(&app).await {
        Some(dir) => app::add_dir(&app, &dir).await.map(Some),
        None => Ok(None),
    }
}

#[tauri::command]
async fn set_start_at_logon(window: WebviewWindow, app: AppHandle, on: bool) -> Result<(), String> {
    main_only(&window)?;
    app::set_tray_autostart(&app, on).await
}

/// Asks (the native dialog) before switching from the Scheduled Tasks.
#[tauri::command]
async fn switch_over(window: WebviewWindow, app: AppHandle) -> Result<(), String> {
    main_only(&window)?;
    let shell = app.state::<Shell>();
    if let Some(refusal) = shell.refusal() {
        return Err(refusal);
    }
    if shell.state().found.is_empty() {
        return Err("No Scheduled Task runs these agents.".to_owned());
    }
    app::confirm_switch_over(&app);
    Ok(())
}

#[tauri::command]
async fn quit(window: WebviewWindow, app: AppHandle) -> Result<(), String> {
    main_only(&window)?;
    app::quit(&app);
    Ok(())
}

/// The page's `prefers-reduced-motion: reduce`.
#[tauri::command]
async fn report_motion(window: WebviewWindow, app: AppHandle, reduce: bool) -> Result<(), String> {
    main_only(&window)?;
    let shell = app.state::<Shell>();
    shell.state().page_reduce_motion = reduce;
    shell.nudge();
    Ok(())
}
