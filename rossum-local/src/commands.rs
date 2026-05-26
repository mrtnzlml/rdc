use crate::connection::Connection;
use crate::state::AppState;
use serde::Serialize;
use tauri::State;

#[derive(Serialize)]
pub struct ConnectionSummary {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub api_base: String,
    pub org_id: u64,
    pub folder: String,
    pub auth_kind: String,
    pub last_sync_unix: Option<i64>,
    pub last_status: String,
    pub last_status_message: Option<String>,
    pub file_count: u64,
}

impl From<&Connection> for ConnectionSummary {
    fn from(c: &Connection) -> Self {
        use crate::connection::ConnectionStatus as S;
        let (status, msg) = match &c.last_status {
            S::Never => ("never", None),
            S::Ok => ("ok", None),
            S::Error(m) => ("error", Some(m.clone())),
        };
        Self {
            id: c.id.to_string(),
            name: c.name.clone(),
            slug: c.slug.clone(),
            api_base: c.api_base.clone(),
            org_id: c.org_id,
            folder: c.folder.display().to_string(),
            auth_kind: match c.auth_kind {
                crate::connection::AuthKind::Token => "token".into(),
                crate::connection::AuthKind::Password => "password".into(),
            },
            last_sync_unix: c.last_sync_unix,
            last_status: status.into(),
            last_status_message: msg,
            file_count: c.file_count,
        }
    }
}

#[derive(Serialize)]
pub struct SettingsResponse {
    pub default_folder_parent: String,
    pub update_channel: String,
    pub app_version: String,
}

#[tauri::command]
pub async fn list_connections(state: State<'_, AppState>) -> Result<Vec<ConnectionSummary>, String> {
    let reg = state.registry.lock().await;
    Ok(reg.connections().iter().map(ConnectionSummary::from).collect())
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<SettingsResponse, String> {
    let s = state.settings.lock().await;
    Ok(SettingsResponse {
        default_folder_parent: s.default_folder_parent.display().to_string(),
        update_channel: match s.update_channel {
            crate::settings::UpdateChannel::Stable => "stable".into(),
            crate::settings::UpdateChannel::Beta => "beta".into(),
        },
        app_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}
