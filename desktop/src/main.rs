use rossum_local::commands;
use rossum_local::state::AppState;
use tauri::menu::{MenuBuilder, SubmenuBuilder};

#[tokio::main]
async fn main() {
    let app_state = AppState::load().expect("loading app state");

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
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
            let app_menu = SubmenuBuilder::new(app, "Rossum Local").quit().build()?;
            // Standard macOS Edit menu so Cmd-X/C/V/A bind to the
            // focused WebView input.
            let edit_menu = SubmenuBuilder::new(app, "Edit")
                .undo()
                .redo()
                .separator()
                .cut()
                .copy()
                .paste()
                .select_all()
                .build()?;
            let menu = MenuBuilder::new(app)
                .item(&app_menu)
                .item(&edit_menu)
                .build()?;
            app.set_menu(menu)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Rossum Local");
}
