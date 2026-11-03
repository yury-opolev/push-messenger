//! The shell around `push-tray-core`: the tray icon (the glyph with a status badge) and its
//! slim native menu, notifications and the menu's dialogs. The supervisor does the work;
//! this module turns its views into a menu, a tooltip and an icon, and menu clicks into
//! supervisor commands or the tray window ([`crate::window`], whose page talks to the
//! commands in [`crate::commands`]).
//!
//! Threading: menu, tray and window events and synchronous commands run on the main thread,
//! and Tauri's menu, tray and window calls from other threads wait for the main thread. So
//! nothing holds [`Shell::state`] while calling into Tauri, only the refresher task touches
//! the tray, the page's commands are all async (off the main thread), and windows are built
//! off the main thread (building a WebView2 window there can deadlock).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use push_bridge_control::instance_id;
use push_tray_core::autostart::Autostart;
use push_tray_core::icon::{self, Frame, Theme};
use push_tray_core::menu::{self, Item, MenuCommand};
use push_tray_core::schedtask::{self, Offer, PowerShellScheduler, TaskFor};
use push_tray_core::state::IconState;
use push_tray_core::supervisor::{
    self, Command, ControlLink, EndpointConnector, InstanceSpec, InstanceView, Notice,
    ProcessSpawner, Supervisor,
};
use push_tray_core::tray_config::TrayConfig;
use push_tray_core::{paths, peek};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, IconMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, Wry};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::{Notify, mpsc, oneshot, watch};

use crate::{commands, theme, window};

const TRAY_ID: &str = "push-agent-tray";
pub const TITLE: &str = "push-agent-tray";
const ICON_SIZE: u32 = 32;
const DOT_SIZE: u32 = 16;
const COMMAND_BUFFER: usize = 64;
/// How many peek entries the menu line is taken from.
const MENU_PEEK_LIMIT: usize = 10;
/// How long the switch-over waits for the supervisor to notice that a task's bridge (gone
/// by then) closed its control channel before starting the instance under the tray.
const SWITCH_WAIT: Duration = Duration::from_secs(5);
pub const SWITCHING: &str = "A switch-over is running. Try again once it has finished.";

/// The app's state (managed by Tauri).
pub struct Shell {
    pub config_path: PathBuf,
    default_dir: PathBuf,
    pub autostart: Box<dyn Autostart + Send + Sync>,
    commands: mpsc::Sender<Command>,
    pub views: watch::Receiver<Vec<InstanceView>>,
    /// Asks the refresher to redraw; `true` also rebuilds the menu.
    refresh: mpsc::UnboundedSender<bool>,
    /// Wakes the menu peek (its setting changed).
    pub peek_wake: Notify,
    supervisor: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    quitting: AtomicBool,
    state: Mutex<TrayState>,
}

pub struct TrayState {
    pub config: TrayConfig,
    /// tray.json could not be read or written.
    pub config_error: Option<String>,
    /// tray.json was read: changes may be saved (a corrupt file is never overwritten).
    pub config_writable: bool,
    /// The Scheduled Tasks that run configured instances.
    pub found: Vec<TaskFor>,
    pub tray_autostart: bool,
    pub switching: bool,
    /// The agent the window should select next (asked for by the menu).
    pub selected: Option<String>,
    /// The page reported `prefers-reduced-motion: reduce`.
    pub page_reduce_motion: bool,
    /// The agents' last lines for the menu (with the menu peek on), by instance id.
    pub menu_lines: HashMap<String, String>,
}

impl Shell {
    pub fn state(&self) -> MutexGuard<'_, TrayState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn send(&self, command: Command) -> Result<(), String> {
        self.commands.try_send(command).map_err(|_| {
            eprintln!("push-agent-tray: the supervisor is busy or gone; command dropped");
            "The tray is busy. Try again.".to_owned()
        })
    }

    /// Redraws the tray and rebuilds its menu.
    pub fn request_refresh(&self) {
        let _ = self.refresh.send(true);
    }

    /// Redraws the tray (the icon and tooltip; the menu only when it changed).
    pub fn nudge(&self) {
        let _ = self.refresh.send(false);
    }

    /// Hands the configured instances (and the tasks that run them) to the supervisor.
    pub fn set_instances(&self) {
        let specs: Vec<InstanceSpec> = {
            let state = self.state();
            supervisor::specs(&state.config, &state.found)
        };
        let _ = self.send(Command::SetInstances(specs));
        self.request_refresh();
    }

    pub fn view(&self, id: &str) -> Option<InstanceView> {
        self.views
            .borrow()
            .iter()
            .find(|view| view.id == id)
            .cloned()
    }

    /// Why tray.json may not be changed now, if it may not.
    pub fn refusal(&self) -> Option<String> {
        let state = self.state();
        if !state.config_writable {
            return Some(self.unwritable());
        }
        state.switching.then(|| SWITCHING.to_owned())
    }

    fn unwritable(&self) -> String {
        format!(
            "{} could not be read, so the tray does not change it. Fix or delete it, then restart the tray.",
            self.config_path.display()
        )
    }

    /// Edits a copy of tray.json with `edit` and saves it when `edit` returns true; refused
    /// when the file could not be read or a switch-over runs.
    pub fn edit_config(
        &self,
        edit: impl FnOnce(&mut TrayConfig) -> Result<bool, String>,
    ) -> Result<bool, String> {
        let mut state = self.state();
        if !state.config_writable {
            return Err(self.unwritable());
        }
        if state.switching {
            return Err(SWITCHING.to_owned());
        }
        let mut config = state.config.clone();
        if !edit(&mut config)? {
            return Ok(false);
        }
        config
            .save(&self.config_path)
            .map_err(|error| format!("tray.json could not be saved: {error}"))?;
        state.config = config;
        Ok(true)
    }
}

pub fn run() {
    let app = tauri::Builder::default()
        // First: a second launch hands over to this one (which shows its window) and exits.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            window::open(app, None);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(commands::handler())
        .on_window_event(window::on_event)
        .setup(|app| {
            setup(app.handle())?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("push-agent-tray could not start");
    app.run(|_, event| {
        // The hidden window does not quit the tray; Quit does (with a code).
        if let RunEvent::ExitRequested {
            code: None, api, ..
        } = event
        {
            api.prevent_exit();
        }
    });
}

fn setup(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    let config_path = paths::tray_json_path();
    let default_dir = paths::default_bridge_dir();
    let state = load_config(&config_path, &default_dir);
    let (supervisor, views, notices) = Supervisor::new(
        Arc::new(ProcessSpawner {
            bridge_exe: supervisor::bridge_exe_next_to(&exe),
        }),
        Arc::new(EndpointConnector),
        Arc::new(PowerShellScheduler),
    );
    let (commands, command_receiver) = mpsc::channel(COMMAND_BUFFER);
    let (refresh, refresh_receiver) = mpsc::unbounded_channel();
    let handle = tauri::async_runtime::spawn(supervisor.run(command_receiver));
    app.manage(Shell {
        config_path,
        default_dir,
        autostart: tray_autostart(exe),
        commands,
        views: views.clone(),
        refresh,
        peek_wake: Notify::new(),
        supervisor: Mutex::new(Some(handle)),
        quitting: AtomicBool::new(false),
        state: Mutex::new(state),
    });
    let theme = theme::current();
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(tray_image(theme, None, Frame::Full))
        .tooltip(TITLE)
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(on_tray_event)
        .build(app)?;
    tauri::async_runtime::spawn(refresher(app.clone(), views.clone(), refresh_receiver));
    tauri::async_runtime::spawn(menu_peek(app.clone(), views));
    tauri::async_runtime::spawn(notify(app.clone(), notices));
    tauri::async_runtime::spawn(begin(app.clone()));
    app.state::<Shell>().request_refresh();
    Ok(())
}

/// tray.json with the default bridge folder added (and saved) when it is set up. A file
/// that cannot be read is reported and left alone: the default folder is supervised, and
/// nothing is saved.
fn load_config(path: &Path, default_dir: &Path) -> TrayState {
    let (config, config_error, config_writable) = match TrayConfig::load(path) {
        Ok(mut config) => {
            let error = if config.ensure_default(default_dir) {
                config.save(path).err().map(|error| error.to_string())
            } else {
                None
            };
            (config, error, true)
        }
        Err(error) => {
            let mut config = TrayConfig::default();
            config.ensure_default(default_dir);
            (config, Some(error.to_string()), false)
        }
    };
    TrayState {
        config,
        config_error,
        config_writable,
        found: Vec::new(),
        tray_autostart: false,
        switching: false,
        selected: None,
        page_reduce_motion: false,
        menu_lines: HashMap::new(),
    }
}

#[cfg(windows)]
fn tray_autostart(exe: PathBuf) -> Box<dyn Autostart + Send + Sync> {
    Box::new(push_tray_core::autostart::RunKey { exe })
}

#[cfg(not(windows))]
fn tray_autostart(exe: PathBuf) -> Box<dyn Autostart + Send + Sync> {
    Box::new(push_tray_core::autostart::XdgAutostart {
        dir: paths::xdg_autostart_dir(),
        exe,
    })
}

/// The Scheduled Tasks that run one of `dirs` (none when they cannot be listed).
#[cfg(windows)]
fn scan(dirs: &[PathBuf], default_dir: &Path) -> Vec<TaskFor> {
    use push_tray_core::schedtask::Scheduler;
    match PowerShellScheduler.list() {
        Ok(tasks) => schedtask::tasks_for_instances(&tasks, dirs, default_dir),
        Err(error) => {
            eprintln!("push-agent-tray: Scheduled Tasks could not be listed: {error}");
            Vec::new()
        }
    }
}

#[cfg(not(windows))]
fn scan(_dirs: &[PathBuf], _default_dir: &Path) -> Vec<TaskFor> {
    Vec::new()
}

fn dirs(config: &TrayConfig) -> Vec<PathBuf> {
    config
        .instances
        .iter()
        .map(|instance| instance.dir.clone())
        .collect()
}

/// Finds the Scheduled Tasks (so that the tray never starts a bridge a task runs) and the
/// tray's own start at logon, then hands the instances to the supervisor, which starts the
/// autostart ones.
async fn begin(app: AppHandle) {
    rescan(&app).await;
    let worker = app.clone();
    let enabled = tauri::async_runtime::spawn_blocking(move || {
        worker.state::<Shell>().autostart.enabled().unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    let shell = app.state::<Shell>();
    shell.state().tray_autostart = enabled;
    shell.set_instances();
}

/// Looks for the Scheduled Tasks that run the configured instances again.
pub async fn rescan(app: &AppHandle) {
    let shell = app.state::<Shell>();
    let dirs = dirs(&shell.state().config);
    let default_dir = shell.default_dir.clone();
    let found = tauri::async_runtime::spawn_blocking(move || scan(&dirs, &default_dir))
        .await
        .unwrap_or_default();
    shell.state().found = found;
}

/// The tray icon's look.
#[derive(Clone, Copy, PartialEq)]
struct Look {
    theme: Theme,
    badge: Option<IconState>,
    frame: Frame,
}

/// What the tray shows.
#[derive(PartialEq)]
struct Shown {
    menu: menu::Menu,
    tooltip: String,
    look: Look,
}

/// Rebuilds the tray from the views: on every change, on request (rebuilding the menu
/// then when asked, so that a check item clicked natively shows the real state again),
/// when the taskbar theme changes (read every [`theme::POLL`]), and every
/// [`icon::PULSE_FRAME`] while the badge pulses.
async fn refresher(
    app: AppHandle,
    mut views: watch::Receiver<Vec<InstanceView>>,
    mut requests: mpsc::UnboundedReceiver<bool>,
) {
    let mut shown: Option<Shown> = None;
    let mut theme = theme::current();
    let mut theme_poll = tokio::time::interval(theme::POLL);
    theme_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pulse = tokio::time::interval(icon::PULSE_FRAME);
    pulse.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut animating = false;
    let mut frame = Frame::Full;
    loop {
        let force = tokio::select! {
            changed = views.changed() => {
                if changed.is_err() {
                    return;
                }
                false
            }
            request = requests.recv() => {
                match request {
                    Some(force) => force,
                    None => return,
                }
            }
            _ = theme_poll.tick() => {
                theme = theme::current();
                false
            }
            _ = pulse.tick(), if animating => {
                frame = match frame {
                    Frame::Full => Frame::Dim,
                    Frame::Dim => Frame::Full,
                };
                false
            }
        };
        let current = views.borrow_and_update().clone();
        let badge = menu::icon(&current);
        let next = {
            let shell = app.state::<Shell>();
            let state = shell.state();
            let reduce_motion = state.config.reduce_motion || state.page_reduce_motion;
            let pulsing = badge.is_some_and(|badge| icon::animated(badge, reduce_motion));
            if pulsing && !animating {
                pulse.reset();
            }
            animating = pulsing;
            if !animating {
                frame = Frame::Full;
            }
            let offer = if state.switching {
                Offer::None
            } else {
                schedtask::offer(&state.found)
            };
            let no_lines = HashMap::new();
            let lines = if state.config.menu_peek {
                &state.menu_lines
            } else {
                &no_lines
            };
            Shown {
                menu: menu::build(
                    &current,
                    state.tray_autostart,
                    &offer,
                    state.config_error.as_deref(),
                    lines,
                ),
                tooltip: menu::tooltip(&current),
                look: Look {
                    theme,
                    badge,
                    frame,
                },
            }
        };
        if let Err(error) = apply(&app, shown.as_ref(), &next, force) {
            eprintln!("push-agent-tray: the tray could not be updated: {error}");
        }
        shown = Some(next);
    }
}

fn apply(app: &AppHandle, shown: Option<&Shown>, next: &Shown, force: bool) -> tauri::Result<()> {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return Ok(());
    };
    if force || shown.is_none_or(|shown| shown.menu != next.menu) {
        tray.set_menu(Some(native_menu(app, &next.menu)?))?;
    }
    if shown.is_none_or(|shown| shown.tooltip != next.tooltip) {
        tray.set_tooltip(Some(&next.tooltip))?;
    }
    if shown.is_none_or(|shown| shown.look != next.look) {
        let look = next.look;
        tray.set_icon(Some(tray_image(look.theme, look.badge, look.frame)))?;
    }
    Ok(())
}

fn tray_image(theme: Theme, badge: Option<IconState>, frame: Frame) -> Image<'static> {
    Image::new_owned(
        icon::tray_rgba(theme, badge, frame, ICON_SIZE),
        ICON_SIZE,
        ICON_SIZE,
    )
}

fn native_menu(app: &AppHandle, model: &menu::Menu) -> tauri::Result<Menu<Wry>> {
    let menu = Menu::new(app)?;
    for item in &model.items {
        match item {
            Item::Agent { id, label, icon } => {
                let dot = Image::new_owned(icon::dot_rgba(*icon, DOT_SIZE), DOT_SIZE, DOT_SIZE);
                menu.append(&IconMenuItem::with_id(
                    app,
                    id.as_str(),
                    label,
                    true,
                    Some(dot),
                    None::<&str>,
                )?)?;
            }
            Item::Action { id, label, enabled } => menu.append(&MenuItem::with_id(
                app,
                id.as_str(),
                label,
                *enabled,
                None::<&str>,
            )?)?,
            Item::Check { id, label, checked } => menu.append(&CheckMenuItem::with_id(
                app,
                id.as_str(),
                label,
                true,
                *checked,
                None::<&str>,
            )?)?,
            Item::Separator => menu.append(&PredefinedMenuItem::separator(app)?)?,
            Item::Info(text) => menu.append(&MenuItem::new(app, text, false, None::<&str>)?)?,
        }
    }
    Ok(menu)
}

/// With the menu peek on: each connected agent's last line (for those whose chat peek is
/// shown), fetched once on connect and again whenever its `peek_changes` grows. Never
/// fetches with the menu peek off.
async fn menu_peek(app: AppHandle, mut views: watch::Receiver<Vec<InstanceView>>) {
    let shell = app.state::<Shell>();
    let mut fetched: HashMap<String, (u64, ControlLink)> = HashMap::new();
    loop {
        let current = views.borrow_and_update().clone();
        let targets: Vec<(String, u64, ControlLink)> = {
            let state = shell.state();
            if state.config.menu_peek {
                current
                    .iter()
                    .filter(|view| {
                        state
                            .config
                            .find(&view.dir)
                            .is_none_or(|instance| instance.show_peek)
                    })
                    .filter_map(|view| {
                        let control = view.control.clone()?;
                        Some((view.id.clone(), view.peek_changes, control))
                    })
                    .collect()
            } else {
                Vec::new()
            }
        };
        fetched.retain(|id, _| targets.iter().any(|(target, _, _)| target == id));
        let mut changed = false;
        for (id, changes, control) in targets {
            if fetched
                .get(&id)
                .is_some_and(|(seen, link)| *seen == changes && *link == control)
            {
                continue;
            }
            let line = control
                .peek(MENU_PEEK_LIMIT)
                .await
                .ok()
                .and_then(|entries| peek::menu_line(&entries));
            fetched.insert(id.clone(), (changes, control));
            let mut state = shell.state();
            let old = match line {
                Some(line) => state.menu_lines.insert(id.clone(), line.clone()) != Some(line),
                None => state.menu_lines.remove(&id).is_some(),
            };
            changed |= old;
        }
        {
            let mut state = shell.state();
            let before = state.menu_lines.len();
            state.menu_lines.retain(|id, _| fetched.contains_key(id));
            changed |= state.menu_lines.len() != before;
        }
        if changed {
            shell.nudge();
        }
        tokio::select! {
            changed = views.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = shell.peek_wake.notified() => {}
        }
    }
}

async fn notify(app: AppHandle, mut notices: mpsc::Receiver<Notice>) {
    while let Some(notice) = notices.recv().await {
        let shown = app
            .notification()
            .builder()
            .title(TITLE)
            .body(push_tray_core::window::notice_text(&notice))
            .show();
        if let Err(error) = shown {
            eprintln!("push-agent-tray: notification failed: {error}");
        }
    }
}

/// An error or information box (does not block).
pub fn message(app: &AppHandle, text: impl Into<String>, kind: MessageDialogKind) {
    app.dialog()
        .message(text)
        .title(TITLE)
        .kind(kind)
        .show(|_| {});
}

fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    match menu::parse(event.id().as_ref()) {
        Some(MenuCommand::OpenAgent(id)) => window::open(app, Some(id)),
        Some(MenuCommand::Open) => window::open(app, None),
        Some(MenuCommand::AddFolder) => add_folder(app),
        Some(MenuCommand::StartAtLogon) => toggle_tray_autostart(app),
        Some(MenuCommand::SwitchOver) => confirm_switch_over(app),
        Some(MenuCommand::Quit) => quit(app),
        None => {}
    }
}

/// A left click opens the window; a right click shows the menu.
fn on_tray_event(tray: &TrayIcon, event: TrayIconEvent) {
    if let TrayIconEvent::Click {
        button: MouseButton::Left,
        button_state: MouseButtonState::Up,
        ..
    } = event
    {
        window::open(tray.app_handle(), None);
    }
}

/// Refuses (with a message box) to change tray.json when it could not be read or a
/// switch-over runs.
fn refuse_menu_edit(app: &AppHandle) -> bool {
    let refusal = app.state::<Shell>().refusal();
    match refusal {
        Some(text) => {
            message(app, text, MessageDialogKind::Error);
            true
        }
        None => false,
    }
}

fn add_folder(app: &AppHandle) {
    if refuse_menu_edit(app) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(dir) = pick_folder(&app).await else {
            return;
        };
        if let Err(error) = add_dir(&app, &dir).await {
            message(&app, error, MessageDialogKind::Error);
        }
    });
}

/// The folder the user picks (none when cancelled).
pub async fn pick_folder(app: &AppHandle) -> Option<PathBuf> {
    let (sender, receiver) = oneshot::channel();
    app.dialog()
        .file()
        .set_title("Add agent folder")
        .pick_folder(move |folder| {
            let _ = sender.send(folder.and_then(|folder| folder.into_path().ok()));
        });
    receiver.await.ok().flatten()
}

/// Adds the bridge folder `dir` to tray.json and the supervisor; its instance id.
pub async fn add_dir(app: &AppHandle, dir: &Path) -> Result<String, String> {
    let added = app.state::<Shell>().edit_config(|config| {
        config
            .add(dir)
            .map_err(|error| format!("Could not add {}: {error}", dir.display()))
    })?;
    if !added {
        return Err(format!("{} is already in the tray.", dir.display()));
    }
    rescan(app).await;
    app.state::<Shell>().set_instances();
    Ok(instance_id(dir))
}

fn toggle_tray_autostart(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let on = !app.state::<Shell>().state().tray_autostart;
        if let Err(error) = set_tray_autostart(&app, on).await {
            message(&app, error, MessageDialogKind::Error);
        }
    });
}

/// Turns the tray's own start at logon on or off.
pub async fn set_tray_autostart(app: &AppHandle, on: bool) -> Result<(), String> {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let shell = app.state::<Shell>();
        let result = shell.autostart.set(on);
        let enabled = shell.autostart.enabled().unwrap_or(on && result.is_ok());
        shell.state().tray_autostart = enabled;
        shell.request_refresh();
        result.map_err(|error| format!("Could not change the start at logon: {error}"))
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Asks before switching from the Scheduled Tasks (a native dialog), then switches.
pub fn confirm_switch_over(app: &AppHandle) {
    if refuse_menu_edit(app) {
        return;
    }
    let found = app.state::<Shell>().state().found.clone();
    if found.is_empty() {
        return;
    }
    let tasks: Vec<&str> = found.iter().map(|item| item.task.as_str()).collect();
    let text = format!(
        "Stop and remove the Scheduled Task(s) {}, start push-agent-tray at logon, and run these agents under the tray instead?",
        tasks.join(", ")
    );
    let handle = app.clone();
    app.dialog()
        .message(text)
        .title(TITLE)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Switch".into(),
            "Cancel".into(),
        ))
        .show(move |confirmed| {
            if confirmed {
                tauri::async_runtime::spawn(switch_over(handle));
            }
        });
}

/// Stops and removes the tasks, turns the tray's start at logon on, marks the instances
/// autostart and saves tray.json, then starts each instance under the tray once its task's
/// bridge is gone.
async fn switch_over(app: AppHandle) {
    let worker = app.clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let shell = worker.state::<Shell>();
        let found = {
            let mut state = shell.state();
            if state.switching {
                return None;
            }
            state.switching = true;
            state.found.clone()
        };
        shell.request_refresh();
        let plan = schedtask::plan_switch(&found);
        let mut started: Vec<PathBuf> = Vec::new();
        let executed = schedtask::execute_switch(
            &plan,
            &PowerShellScheduler,
            shell.autostart.as_ref(),
            &mut |dir| started.push(dir.to_path_buf()),
        );
        let tray_autostart = shell.autostart.enabled().unwrap_or(false);
        // The config may have changed during the slow steps: edit the current one.
        let (saved, scan_dirs) = {
            let mut state = shell.state();
            let saved = match executed {
                Ok(()) => {
                    let mut config = state.config.clone();
                    schedtask::apply_switch(&plan, &mut config);
                    let saved = config.save(&shell.config_path).map_err(io::Error::other);
                    state.config = config;
                    saved
                }
                Err(error) => Err(error),
            };
            (saved, dirs(&state.config))
        };
        let found = scan(&scan_dirs, &shell.default_dir);
        {
            let mut state = shell.state();
            state.found = found;
            state.tray_autostart = tray_autostart;
            state.switching = false;
        }
        Some((saved, started))
    })
    .await;
    let shell = app.state::<Shell>();
    let (saved, started) = match outcome {
        Ok(Some(outcome)) => outcome,
        Ok(None) => return,
        Err(error) => (Err(io::Error::other(error.to_string())), Vec::new()),
    };
    shell.set_instances();
    let mut stuck: Vec<String> = Vec::new();
    for dir in started {
        // Never both: a task's bridge that survives its task is asked to shut down, and
        // the tray starts its own only once it is gone.
        if let Err(error) =
            supervisor::retire_task_bridge(&EndpointConnector, &dir, supervisor::SWITCH_PATIENCE)
                .await
        {
            stuck.push(error.to_string());
            continue;
        }
        let id = instance_id(&dir);
        let mut views = shell.views.clone();
        let gone =
            views.wait_for(|views| views.iter().any(|view| view.id == id && !view.connected));
        let _ = tokio::time::timeout(SWITCH_WAIT, gone).await;
        let _ = shell.send(Command::Start(dir));
    }
    if let Err(error) = saved {
        message(
            &app,
            format!("The switch-over stopped: {error}"),
            MessageDialogKind::Error,
        );
    }
    if !stuck.is_empty() {
        message(
            &app,
            format!(
                "Not started under the tray: {}. Stop it, then use Start.",
                stuck.join("; ")
            ),
            MessageDialogKind::Error,
        );
    }
}

/// Stops the bridges the tray started (the others keep running), then exits.
pub fn quit(app: &AppHandle) {
    let shell = app.state::<Shell>();
    if shell.quitting.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let shell = app.state::<Shell>();
        let _ = shell.commands.send(Command::Quit).await;
        let handle = shell
            .supervisor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
        app.exit(0);
    });
}
