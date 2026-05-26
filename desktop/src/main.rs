use rossum_local::commands;
use rossum_local::state::AppState;
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
use tauri::{Emitter, Manager};
#[cfg(target_os = "macos")]
use tauri::{
    utils::config::WindowEffectsConfig,
    window::{Effect, EffectState},
};

#[tokio::main]
async fn main() {
    let app_state = AppState::load().expect("loading app state");

    tauri::Builder::default()
        // Single-instance: a second app launch (double-click Dock icon
        // while one window is open) just refocuses the existing window.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::list_connections,
            commands::add_connection,
            commands::open_existing_project,
            commands::sync_connection,
            commands::edit_credentials,
            commands::remove_connection,
            commands::reveal_folder,
        ])
        .setup(|app| {
            // Real macOS vibrancy: the Tauri window is transparent and we
            // apply the same NSVisualEffectView material Mail / Music /
            // Notes use on their sidebars.
            #[cfg(target_os = "macos")]
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.set_effects(WindowEffectsConfig {
                    effects: vec![Effect::Sidebar],
                    state: Some(EffectState::FollowsWindowActiveState),
                    radius: None,
                    color: None,
                });
            }

            // Standard macOS menu bar. Custom IDs are emitted to the
            // frontend as Tauri events; built-in items (cut/copy/paste,
            // minimize/zoom, close-window) use PredefinedMenuItem so the
            // OS handles them.
            let new_conn = MenuItemBuilder::new("New Connection…")
                .id("menu:new-connection")
                .accelerator("Cmd+N")
                .build(app)?;
            let open_existing = MenuItemBuilder::new("Open Existing rdc Project…")
                .id("menu:open-existing")
                .accelerator("Cmd+O")
                .build(app)?;

            let app_menu = SubmenuBuilder::new(app, "Rossum Local")
                .item(&PredefinedMenuItem::about(app, None, None)?)
                .separator()
                .item(&PredefinedMenuItem::hide(app, None)?)
                .item(&PredefinedMenuItem::hide_others(app, None)?)
                .item(&PredefinedMenuItem::show_all(app, None)?)
                .separator()
                .quit()
                .build()?;

            let file_menu = SubmenuBuilder::new(app, "File")
                .item(&new_conn)
                .item(&open_existing)
                .separator()
                .item(&PredefinedMenuItem::close_window(app, None)?)
                .build()?;

            // Edit menu wires the standard Cmd-X/C/V/A shortcuts to the
            // focused WebView input. Without it, copy/paste silently fail.
            let edit_menu = SubmenuBuilder::new(app, "Edit")
                .undo()
                .redo()
                .separator()
                .cut()
                .copy()
                .paste()
                .select_all()
                .build()?;

            let view_menu = SubmenuBuilder::new(app, "View")
                .item(
                    &MenuItemBuilder::new("Reload")
                        .id("menu:reload")
                        .accelerator("Cmd+R")
                        .build(app)?,
                )
                .build()?;

            let window_menu = SubmenuBuilder::new(app, "Window")
                .item(&PredefinedMenuItem::minimize(app, None)?)
                .item(&PredefinedMenuItem::maximize(app, Some("Zoom"))?)
                .build()?;

            let menu = MenuBuilder::new(app)
                .item(&app_menu)
                .item(&file_menu)
                .item(&edit_menu)
                .item(&view_menu)
                .item(&window_menu)
                .build()?;
            app.set_menu(menu)?;
            Ok(())
        })
        .on_menu_event(|app, event| {
            let id = event.id().as_ref();
            match id {
                "menu:new-connection" | "menu:open-existing" => {
                    let _ = app.emit(id, ());
                }
                "menu:reload" => {
                    if let Some(win) = app.get_webview_window("main") {
                        let _ = win.eval("window.location.reload()");
                    }
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Rossum Local");
}
