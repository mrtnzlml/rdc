use rossum_local::commands;
use rossum_local::state::AppState;
use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::Emitter;

#[tokio::main]
async fn main() {
    let app_state = AppState::load().expect("loading app state");

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::list_connections,
            commands::get_settings,
            commands::add_connection,
            commands::sync_connection,
            commands::edit_credentials,
            commands::remove_connection,
            commands::update_settings,
            commands::get_diagnostics,
        ])
        .setup(|app| {
            let settings = MenuItemBuilder::new("Settings…")
                .id("open-settings")
                .accelerator("Cmd+,")
                .build(app)?;
            let diagnostics = MenuItemBuilder::new("Diagnostics…")
                .id("open-diagnostics")
                .build(app)?;
            let app_menu = SubmenuBuilder::new(app, "Rossum Local")
                .item(&settings)
                .separator()
                .item(&diagnostics)
                .separator()
                .quit()
                .build()?;
            let menu = MenuBuilder::new(app).item(&app_menu).build()?;
            app.set_menu(menu)?;
            Ok(())
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open-settings" => {
                let _ = app.emit("open-settings", ());
            }
            "open-diagnostics" => {
                let _ = app.emit("open-diagnostics", ());
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running Rossum Local");
}
