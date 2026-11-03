//! The tray window (label [`LABEL`], the bundled `index.html`): opened from the tray icon,
//! the menu or a second launch; closing it hides it and saves its place in tray.json.

use push_tray_core::tray_config::WindowPlace;
use tauri::{
    AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_dialog::MessageDialogKind;

use crate::app::{Shell, TITLE, message};

pub const LABEL: &str = "main";
const PAGE: &str = "index.html";
const SIZE: (f64, f64) = (900.0, 600.0);
const MIN_SIZE: (f64, f64) = (640.0, 420.0);

/// Shows and focuses the window (building it when needed), on agent `select` when given.
pub fn open(app: &AppHandle, select: Option<String>) {
    if let Some(id) = select {
        app.state::<Shell>().state().selected = Some(id);
    }
    if let Some(window) = app.get_webview_window(LABEL) {
        show(&window);
        return;
    }
    let app = app.clone();
    // Built off the main thread: building a WebView2 window there can deadlock.
    tauri::async_runtime::spawn(async move {
        if let Err(error) = build(&app) {
            message(
                &app,
                format!("Could not open the window: {error}"),
                MessageDialogKind::Error,
            );
        }
    });
}

fn show(window: &WebviewWindow) {
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
}

fn build(app: &AppHandle) -> tauri::Result<()> {
    // Another open may have built it meanwhile.
    if let Some(window) = app.get_webview_window(LABEL) {
        show(&window);
        return Ok(());
    }
    let place = app.state::<Shell>().state().config.window;
    let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App(PAGE.into()))
        .title(TITLE)
        .inner_size(SIZE.0, SIZE.1)
        .min_inner_size(MIN_SIZE.0, MIN_SIZE.1)
        .resizable(true)
        .visible(false)
        .build()?;
    if let Some(place) = place.filter(|place| usable(app, place)) {
        window.set_size(PhysicalSize::new(place.width, place.height))?;
        window.set_position(PhysicalPosition::new(place.x, place.y))?;
    }
    show(&window);
    Ok(())
}

/// A saved place at least the minimum size whose title bar is on a monitor.
fn usable(app: &AppHandle, place: &WindowPlace) -> bool {
    if f64::from(place.width) < MIN_SIZE.0 || f64::from(place.height) < MIN_SIZE.1 {
        return false;
    }
    let x = i64::from(place.x) + i64::from(place.width) / 2;
    let y = i64::from(place.y) + 16;
    app.available_monitors().is_ok_and(|monitors| {
        monitors.iter().any(|monitor| {
            let (origin, size) = (monitor.position(), monitor.size());
            let (left, top) = (i64::from(origin.x), i64::from(origin.y));
            (left..left + i64::from(size.width)).contains(&x)
                && (top..top + i64::from(size.height)).contains(&y)
        })
    })
}

/// Closing the window hides it (the tray keeps running) and saves its place.
pub fn on_event(window: &tauri::Window, event: &WindowEvent) {
    if window.label() != LABEL {
        return;
    }
    let WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    api.prevent_close();
    let place = match (
        window.is_minimized(),
        window.outer_position(),
        window.inner_size(),
    ) {
        (Ok(false), Ok(position), Ok(size)) => Some(WindowPlace {
            x: position.x,
            y: position.y,
            width: size.width,
            height: size.height,
        }),
        _ => None,
    };
    let _ = window.hide();
    if let Some(place) = place {
        let app = window.app_handle().clone();
        tauri::async_runtime::spawn_blocking(move || save_place(&app, place));
    }
}

/// Saves the window's place when tray.json may be written.
fn save_place(app: &AppHandle, place: WindowPlace) {
    let shell = app.state::<Shell>();
    let mut state = shell.state();
    if !state.config_writable || state.config.window == Some(place) {
        return;
    }
    let mut config = state.config.clone();
    config.window = Some(place);
    match config.save(&shell.config_path) {
        Ok(()) => state.config = config,
        Err(error) => eprintln!("push-agent-tray: the window's place was not saved: {error}"),
    }
}
