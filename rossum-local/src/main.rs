use rossum_local::commands;
use rossum_local::state::AppState;

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
        ])
        .run(tauri::generate_context!())
        .expect("error while running Rossum Local");
}
