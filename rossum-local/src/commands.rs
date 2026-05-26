use crate::auth;
use crate::connection::{AuthKind, Connection, ConnectionStatus};
use crate::keychain::{Keychain, TokenEntry};
use crate::state::AppState;
use crate::url_normalize::normalize_api_base;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::State;
use ulid::Ulid;

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

#[derive(Debug, Clone, Deserialize)]
pub struct AddConnectionInput {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: String, // "token" | "password"
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub folder: Option<String>,
}

/// Validate the input against a live Rossum endpoint. Returns the bearer
/// token to store (either the user-pasted token or the token issued by
/// `/auth/login` for password mode). Errors are user-facing English
/// messages.
pub async fn validate_add_input_against_rossum(
    input: &AddConnectionInput,
) -> Result<String, String> {
    let api_base_raw = input.api_base.trim_end_matches('/');
    let token = match input.auth_kind.as_str() {
        "token" => input
            .token
            .clone()
            .ok_or_else(|| "Token is required.".to_string())?,
        "password" => {
            let u = input
                .username
                .clone()
                .ok_or_else(|| "Username is required.".to_string())?;
            let p = input
                .password
                .clone()
                .ok_or_else(|| "Password is required.".to_string())?;
            auth::login(api_base_raw, &u, &p)
                .await
                .map_err(|e| e.to_string())?
        }
        other => return Err(format!("Unknown auth_kind '{other}'.")),
    };

    let url = format!("{}/organizations/{}", api_base_raw, input.org_id);
    let resp = reqwest::Client::new()
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| format!("Couldn't reach Rossum: {e}"))?;
    match resp.status().as_u16() {
        200 => Ok(token),
        401 | 403 => Err("Sign-in failed. Check your token and try again.".into()),
        404 => Err(format!("Organization {} not found on this URL.", input.org_id)),
        s => Err(format!("Rossum returned {s}; try again later.")),
    }
}

#[tauri::command]
pub async fn add_connection(
    state: State<'_, AppState>,
    input: AddConnectionInput,
) -> Result<ConnectionSummary, String> {
    let api_base = normalize_api_base(&input.api_base).map_err(|e| e.to_string())?;
    let token = validate_add_input_against_rossum(&input).await?;

    let mut reg = state.registry.lock().await;
    let used = reg.used_slugs();
    let slug = crate::slug::derive_slug(&input.name, &used);
    let id = Ulid::new();

    let folder = match input.folder.clone() {
        Some(f) => PathBuf::from(f),
        None => state
            .settings
            .lock()
            .await
            .default_folder_parent
            .join(&slug),
    };
    std::fs::create_dir_all(&folder).map_err(|e| format!("Creating folder: {e}"))?;

    let auth_kind = match input.auth_kind.as_str() {
        "token" => AuthKind::Token,
        "password" => AuthKind::Password,
        other => return Err(format!("Unknown auth_kind '{other}'.")),
    };

    let token_entry = match auth_kind {
        AuthKind::Token => TokenEntry {
            token,
            expires_at_unix: None,
        },
        AuthKind::Password => TokenEntry {
            token,
            expires_at_unix: Some(now_unix() + 162 * 3600),
        },
    };
    state
        .keychain
        .put_token(id, &token_entry)
        .map_err(|e| format!("Keychain write: {e:#}"))?;
    if matches!(auth_kind, AuthKind::Password) {
        state
            .keychain
            .put_credentials(
                id,
                input.username.as_deref().unwrap(),
                input.password.as_deref().unwrap(),
            )
            .map_err(|e| format!("Keychain write: {e:#}"))?;
    }

    let conn = Connection {
        id,
        name: input.name.clone(),
        slug,
        api_base,
        org_id: input.org_id,
        folder,
        auth_kind,
        last_sync_unix: None,
        last_status: ConnectionStatus::Never,
        file_count: 0,
    };
    reg.upsert(conn.clone());
    reg.save(&state.registry_path)
        .map_err(|e| format!("Saving registry: {e:#}"))?;
    Ok((&conn).into())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
