//! The shell around `push-tray-core`: the tray icon and its native menu, the details
//! windows, notifications and dialogs. The supervisor does the work; this module turns its
//! views into a menu, a tooltip and an icon, and menu clicks into supervisor commands.
//!
//! Threading: menu events and synchronous commands run on the main thread, and Tauri's
//! menu and tray calls from other threads wait for the main thread. So nothing holds
//! [`Shell::state`] while calling into Tauri, and only the refresher task touches the tray.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use push_bridge_control::instance_id;
use push_tray_core::autostart::Autostart;
use push_tray_core::details::{self, DetailsView};
use push_tray_core::menu::{self, Item, MenuAction};
use push_tray_core::schedtask::{self, Offer, PowerShellScheduler, TaskFor};
use push_tray_core::state::IconState;
use push_tray_core::supervisor::{
    self, Command, EndpointConnector, InstanceSpec, InstanceView, Notice, ProcessSpawner,
    Supervisor,
};
use push_tray_core::tray_config::TrayConfig;
use push_tray_core::{icon, open::open_path, paths};
use tauri::image::Image;
use tauri::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, Wry};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::{mpsc, watch};

const TRAY_ID: &str = "push-agent-tray";
const TITLE: &str = "push-agent-tray";
const ICON_SIZE: u32 = 32;
const COMMAND_BUFFER: usize = 64;
/// How long the switch-over waits for a task's bridge to go away before starting it under
/// the tray.
const SWITCH_WAIT: Duration = Duration::from_secs(15);
const DETAILS_PAGE: &str = "details.html";
const DETAILS_SIZE: (f64, f64) = (420.0, 520.0);

/// The app's state (managed by Tauri).
struct Shell {
    config_path: PathBuf,
    default_dir: PathBuf,
    autostart: Box<dyn Autostart + Send + Sync>,
    commands: mpsc::Sender<Command>,
    views: watch::Receiver<Vec<InstanceView>>,
    refresh: mpsc::UnboundedSender<()>,
    supervisor: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    quitting: AtomicBool,
    state: Mutex<TrayState>,
}

struct TrayState {
    config: TrayConfig,
    /// tray.json could not be read or written.
    config_error: Option<String>,
    /// tray.json was read: changes may be saved (a corrupt file is never overwritten).
    config_writable: bool,
    /// The Scheduled Tasks that run configured instances.
    found: Vec<TaskFor>,
    tray_autostart: bool,
    switching: bool,
}

impl Shell {
    fn state(&self) -> MutexGuard<'_, TrayState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, command: Command) {
        if self.commands.try_send(command).is_err() {
            eprintln!("push-agent-tray: the supervisor is busy or gone; command dropped");
        }
    }

    fn request_refresh(&self) {
        let _ = self.refresh.send(());
    }

    /// Hands the configured instances (and the tasks that run them) to the supervisor.
    fn set_instances(&self) {
        let specs: Vec<InstanceSpec> = {
            let state = self.state();
            supervisor::specs(&state.config, &state.found)
        };
        self.send(Command::SetInstances(specs));
        self.request_refresh();
    }

    fn view(&self, id: &str) -> Option<InstanceView> {
        self.views
            .borrow()
            .iter()
            .find(|view| view.id == id)
            .cloned()
    }
}

pub fn run() {
    let app = tauri::Builder::default()
        // First: a second launch hands over to this one and exits.
        .plugin(tauri_plugin_single_instance::init(|_, _, _| {}))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![details])
        .setup(|app| {
            setup(app.handle())?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("push-agent-tray could not start");
    app.run(|_, event| {
        // Closing the last details window does not quit the tray; Quit does (with a code).
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
        supervisor: Mutex::new(Some(handle)),
        quitting: AtomicBool::new(false),
        state: Mutex::new(state),
    });
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(tray_image(IconState::Normal))
        .tooltip(TITLE)
        .show_menu_on_left_click(true)
        .on_menu_event(on_menu_event)
        .build(app)?;
    tauri::async_runtime::spawn(refresher(app.clone(), views, refresh_receiver));
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
async fn rescan(app: &AppHandle) {
    let shell = app.state::<Shell>();
    let dirs = dirs(&shell.state().config);
    let default_dir = shell.default_dir.clone();
    let found = tauri::async_runtime::spawn_blocking(move || scan(&dirs, &default_dir))
        .await
        .unwrap_or_default();
    shell.state().found = found;
}

/// What the tray shows.
#[derive(PartialEq)]
struct Shown {
    menu: menu::Menu,
    tooltip: String,
    icon: IconState,
}

/// Rebuilds the tray from the views: on every change, and on request (always rebuilding
/// the menu then, so that a check item clicked natively shows the real state again).
async fn refresher(
    app: AppHandle,
    mut views: watch::Receiver<Vec<InstanceView>>,
    mut requests: mpsc::UnboundedReceiver<()>,
) {
    let mut shown: Option<Shown> = None;
    loop {
        let force = tokio::select! {
            changed = views.changed() => {
                if changed.is_err() {
                    return;
                }
                false
            }
            request = requests.recv() => {
                if request.is_none() {
                    return;
                }
                true
            }
        };
        let current = views.borrow_and_update().clone();
        let next = {
            let shell = app.state::<Shell>();
            let state = shell.state();
            let offer = if state.switching {
                Offer::None
            } else {
                schedtask::offer(&state.found)
            };
            Shown {
                menu: menu::build(
                    &current,
                    state.tray_autostart,
                    &offer,
                    state.config_error.as_deref(),
                ),
                tooltip: menu::tooltip(&current),
                icon: menu::icon(&current),
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
    if shown.is_none_or(|shown| shown.icon != next.icon) {
        tray.set_icon(Some(tray_image(next.icon)))?;
    }
    Ok(())
}

fn tray_image(state: IconState) -> Image<'static> {
    Image::new_owned(icon::rgba(state, ICON_SIZE), ICON_SIZE, ICON_SIZE)
}

fn native_menu(app: &AppHandle, model: &menu::Menu) -> tauri::Result<Menu<Wry>> {
    let menu = Menu::new(app)?;
    for item in &model.items {
        append(app, item, &|native| menu.append(native))?;
    }
    Ok(menu)
}

type Append<'a> = &'a dyn Fn(&dyn IsMenuItem<Wry>) -> tauri::Result<()>;

fn append(app: &AppHandle, item: &Item, to: Append<'_>) -> tauri::Result<()> {
    match item {
        Item::Action { id, label, enabled } => to(&MenuItem::with_id(
            app,
            id.as_str(),
            label,
            *enabled,
            None::<&str>,
        )?),
        Item::Check { id, label, checked } => to(&CheckMenuItem::with_id(
            app,
            id.as_str(),
            label,
            true,
            *checked,
            None::<&str>,
        )?),
        Item::Separator => to(&PredefinedMenuItem::separator(app)?),
        Item::Info(text) => to(&MenuItem::new(app, text, false, None::<&str>)?),
        Item::Submenu { label, items } => {
            let submenu = Submenu::new(app, label, true)?;
            for child in items {
                append(app, child, &|native| submenu.append(native))?;
            }
            to(&submenu)
        }
    }
}

async fn notify(app: AppHandle, mut notices: mpsc::Receiver<Notice>) {
    while let Some(notice) = notices.recv().await {
        let shown = app
            .notification()
            .builder()
            .title(TITLE)
            .body(details::notice_text(&notice))
            .show();
        if let Err(error) = shown {
            eprintln!("push-agent-tray: notification failed: {error}");
        }
    }
}

/// An error or information box (does not block).
fn message(app: &AppHandle, text: impl Into<String>, kind: MessageDialogKind) {
    app.dialog()
        .message(text)
        .title(TITLE)
        .kind(kind)
        .show(|_| {});
}

fn on_menu_event(app: &AppHandle, event: MenuEvent) {
    let id: &str = event.id().as_ref();
    match id {
        menu::ADD_FOLDER => add_folder(app),
        menu::START_AT_LOGON => toggle_tray_autostart(app),
        menu::SWITCH_OVER => confirm_switch_over(app),
        menu::QUIT => quit(app),
        _ => {
            if let Some((action, instance)) = menu::parse_id(id) {
                agent_action(app, action, &instance);
            }
        }
    }
}

fn agent_action(app: &AppHandle, action: MenuAction, instance: &str) {
    let shell = app.state::<Shell>();
    let Some(view) = shell.view(instance) else {
        return;
    };
    let dir = view.dir.clone();
    match action {
        MenuAction::Start => shell.send(Command::Start(dir)),
        MenuAction::Stop => shell.send(Command::Stop(dir)),
        MenuAction::RestartBackend => shell.send(Command::RestartBackend(dir)),
        MenuAction::Reconnect => shell.send(Command::Reconnect(dir)),
        MenuAction::Pause => shell.send(Command::Pause(dir)),
        MenuAction::Resume => shell.send(Command::Resume(dir)),
        MenuAction::OpenLog => {
            let log = menu::log_path(&dir);
            if log.is_file() {
                open(app, &log);
            } else {
                message(
                    app,
                    format!("{} has no log yet ({}).", view.name, log.display()),
                    MessageDialogKind::Info,
                );
            }
        }
        MenuAction::OpenFolder => open(app, &dir),
        MenuAction::Details => show_details(app, &view),
    }
}

fn open(app: &AppHandle, path: &Path) {
    if let Err(error) = open_path(path) {
        message(
            app,
            format!("Could not open {}: {error}", path.display()),
            MessageDialogKind::Error,
        );
    }
}

/// The agent's details window, focused when it is already open.
fn show_details(app: &AppHandle, view: &InstanceView) {
    let label = details::window_label(&view.id);
    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }
    let title = details::window_title(&view.name);
    let app = app.clone();
    // Built off the main thread: building a WebView2 window there can deadlock.
    tauri::async_runtime::spawn(async move {
        let built = WebviewWindowBuilder::new(&app, label, WebviewUrl::App(DETAILS_PAGE.into()))
            .title(title)
            .inner_size(DETAILS_SIZE.0, DETAILS_SIZE.1)
            .build();
        if let Err(error) = built {
            message(
                &app,
                format!("Could not open the details: {error}"),
                MessageDialogKind::Error,
            );
        }
    });
}

/// The `details` command: the details of the agent whose window asks. Content-free.
#[tauri::command]
fn details(
    window: tauri::WebviewWindow,
    shell: tauri::State<'_, Shell>,
) -> Result<DetailsView, String> {
    let id = details::instance_of_window(window.label())
        .ok_or_else(|| "not a details window".to_owned())?;
    shell
        .view(id)
        .map(|view| details::details_view(&view))
        .ok_or_else(|| "this agent is no longer in the tray".to_owned())
}

/// Refuses to change tray.json when it could not be read (it is never overwritten).
fn config_writable(app: &AppHandle) -> bool {
    let shell = app.state::<Shell>();
    let writable = shell.state().config_writable;
    if !writable {
        message(
            app,
            format!(
                "{} could not be read, so the tray does not change it. Fix or delete it, then restart the tray.",
                shell.config_path.display()
            ),
            MessageDialogKind::Error,
        );
    }
    writable
}

fn add_folder(app: &AppHandle) {
    if !config_writable(app) {
        return;
    }
    let handle = app.clone();
    app.dialog()
        .file()
        .set_title("Add agent folder")
        .pick_folder(move |folder| {
            let Some(dir) = folder.and_then(|folder| folder.into_path().ok()) else {
                return;
            };
            tauri::async_runtime::spawn(add_dir(handle, dir));
        });
}

async fn add_dir(app: AppHandle, dir: PathBuf) {
    let added = {
        let shell = app.state::<Shell>();
        let mut state = shell.state();
        let mut config = state.config.clone();
        match config.add(&dir) {
            Ok(true) => config.save(&shell.config_path).map(|()| {
                state.config = config;
                true
            }),
            other => other,
        }
    };
    match added {
        Ok(true) => {
            rescan(&app).await;
            app.state::<Shell>().set_instances();
        }
        Ok(false) => message(
            &app,
            format!("{} is already in the tray.", dir.display()),
            MessageDialogKind::Info,
        ),
        Err(error) => message(
            &app,
            format!("Could not add {}: {error}", dir.display()),
            MessageDialogKind::Error,
        ),
    }
}

fn toggle_tray_autostart(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let shell = app.state::<Shell>();
        let on = !shell.state().tray_autostart;
        let result = shell.autostart.set(on);
        let enabled = shell.autostart.enabled().unwrap_or(on && result.is_ok());
        shell.state().tray_autostart = enabled;
        shell.request_refresh();
        if let Err(error) = result {
            message(
                &app,
                format!("Could not change the start at logon: {error}"),
                MessageDialogKind::Error,
            );
        }
    });
}

fn confirm_switch_over(app: &AppHandle) {
    if !config_writable(app) {
        return;
    }
    let found = {
        let shell = app.state::<Shell>();
        let state = shell.state();
        if state.switching {
            return;
        }
        state.found.clone()
    };
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
        let (mut config, found) = {
            let mut state = shell.state();
            if state.switching {
                return None;
            }
            state.switching = true;
            (state.config.clone(), state.found.clone())
        };
        shell.request_refresh();
        let plan = schedtask::plan_switch(&found);
        let mut started: Vec<PathBuf> = Vec::new();
        let executed = schedtask::execute_switch(
            &plan,
            &PowerShellScheduler,
            shell.autostart.as_ref(),
            &mut config,
            &mut |dir| started.push(dir.to_path_buf()),
        );
        let executed_ok = executed.is_ok();
        let saved = match executed {
            Ok(()) => config.save(&shell.config_path).map_err(io::Error::other),
            Err(error) => Err(error),
        };
        let found = scan(&dirs(&config), &shell.default_dir);
        let tray_autostart = shell.autostart.enabled().unwrap_or(false);
        {
            let mut state = shell.state();
            if executed_ok {
                state.config = config;
            }
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
    for dir in started {
        let id = instance_id(&dir);
        let mut views = shell.views.clone();
        let gone =
            views.wait_for(|views| views.iter().any(|view| view.id == id && !view.connected));
        let _ = tokio::time::timeout(SWITCH_WAIT, gone).await;
        shell.send(Command::Start(dir));
    }
    if let Err(error) = saved {
        message(
            &app,
            format!("The switch-over stopped: {error}"),
            MessageDialogKind::Error,
        );
    }
}

/// Stops the bridges the tray started (the others keep running), then exits.
fn quit(app: &AppHandle) {
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
