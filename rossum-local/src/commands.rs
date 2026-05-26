use crate::auth;
use crate::connection::{AuthKind, Connection, ConnectionStatus};
use crate::keychain::{Keychain, TokenEntry};
use crate::state::AppState;
use crate::sync::{run_sync, SyncError};
use crate::url_normalize::normalize_api_base;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, State};
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
///
/// `api_base` must already be normalized (via `normalize_api_base`).
pub async fn validate_add_input_against_rossum(
    input: &AddConnectionInput,
    api_base: &str,
) -> Result<String, String> {
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
            auth::login(api_base, &u, &p)
                .await
                .map_err(|e| e.to_string())?
        }
        other => return Err(format!("Unknown auth_kind '{other}'.")),
    };

    let url = format!("{}/organizations/{}", api_base, input.org_id);
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
    let token = validate_add_input_against_rossum(&input, &api_base).await?;

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
            expires_at_unix: Some(now_unix() + crate::auth::TOKEN_LIFETIME_SECS),
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

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Serialize, Clone)]
pub struct SyncProgress {
    pub connection_id: String,
    pub phase: String, // "started" | "done" | "error"
    pub message: Option<String>,
    pub file_count: Option<u64>,
}

#[tauri::command]
pub async fn sync_connection(
    app: AppHandle,
    state: State<'_, AppState>,
    connection_id: String,
) -> Result<(), String> {
    let id: Ulid = connection_id
        .parse()
        .map_err(|_| "Bad connection id".to_string())?;
    let conn = state
        .registry
        .lock()
        .await
        .get(id)
        .cloned()
        .ok_or_else(|| "Connection not found".to_string())?;

    let kc = state.keychain.clone();
    let registry_arc = state.registry.clone();
    let registry_path = state.registry_path.clone();
    let diag = state.diag.clone();

    let _ = app.emit(
        "sync-progress",
        SyncProgress {
            connection_id: id.to_string(),
            phase: "started".into(),
            message: None,
            file_count: None,
        },
    );
    diag.push(format!("sync start: {}", conn.name));

    let app2 = app.clone();
    let conn2 = conn.clone();

    state
        .queue
        .submit(id, async move {
            // run_sync is !Send (rdc internals hold StdinLock across
            // await points). Offload to a blocking thread that runs its
            // own mini async executor so we stay on the SyncQueue's
            // Send + 'static contract.
            let kc2 = kc.clone();
            let conn3 = conn2.clone();
            let result = tokio::task::spawn_blocking(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build rt")
                    .block_on(run_sync(&conn3, &*kc2))
            })
            .await
            .unwrap_or_else(|e| Err(SyncError::Other(format!("task panicked: {e}"))));

            // Update registry with outcome.
            let mut reg = registry_arc.lock().await;
            let mut new_conn = conn2.clone();
            match &result {
                Ok(outcome) => {
                    new_conn.last_sync_unix = Some(now_unix());
                    new_conn.last_status = ConnectionStatus::Ok;
                    new_conn.file_count = outcome.file_count;
                    diag.push(format!(
                        "sync done: {} ({} files)",
                        conn2.name, outcome.file_count
                    ));
                }
                Err(e) => {
                    new_conn.last_status = ConnectionStatus::Error(format!("{e}"));
                    diag.push(format!("sync error: {}: {}", conn2.name, e));
                }
            }
            // Defensive: skip the upsert if remove_connection ran while we were syncing.
            // The registry's `remove` method clears the id; if `get` returns None, the
            // connection was removed and we should not re-create it.
            if reg.get(id).is_some() {
                reg.upsert(new_conn);
                let _ = reg.save(&registry_path);
            }
            drop(reg);

            let progress = match &result {
                Ok(o) => SyncProgress {
                    connection_id: id.to_string(),
                    phase: "done".into(),
                    message: None,
                    file_count: Some(o.file_count),
                },
                Err(e) => SyncProgress {
                    connection_id: id.to_string(),
                    phase: "error".into(),
                    message: Some(format!("{e}")),
                    file_count: None,
                },
            };
            let _ = app2.emit("sync-progress", progress);
        })
        .map_err(|e| format!("{e:#}"))?;

    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct EditCredentialsInput {
    pub connection_id: String,
    pub auth_kind: String, // "token" | "password"
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[tauri::command]
pub async fn edit_credentials(
    state: State<'_, AppState>,
    input: EditCredentialsInput,
) -> Result<(), String> {
    let id: Ulid = input.connection_id.parse().map_err(|_| "Bad id".to_string())?;
    let mut reg = state.registry.lock().await;
    let mut conn = reg
        .get(id)
        .cloned()
        .ok_or_else(|| "Connection not found".to_string())?;

    let validation_input = AddConnectionInput {
        name: conn.name.clone(),
        api_base: conn.api_base.clone(),
        org_id: conn.org_id,
        auth_kind: input.auth_kind.clone(),
        token: input.token.clone(),
        username: input.username.clone(),
        password: input.password.clone(),
        folder: Some(conn.folder.display().to_string()),
    };
    let new_token = validate_add_input_against_rossum(&validation_input, &conn.api_base).await?;

    let new_kind = match input.auth_kind.as_str() {
        "token" => AuthKind::Token,
        "password" => AuthKind::Password,
        other => return Err(format!("Unknown auth_kind '{other}'.")),
    };
    let entry = match new_kind {
        AuthKind::Token => TokenEntry { token: new_token, expires_at_unix: None },
        AuthKind::Password => TokenEntry {
            token: new_token,
            expires_at_unix: Some(now_unix() + crate::auth::TOKEN_LIFETIME_SECS),
        },
    };
    state
        .keychain
        .delete_all(id)
        .map_err(|e| format!("Clearing old credentials: {e:#}"))?;
    state
        .keychain
        .put_token(id, &entry)
        .map_err(|e| format!("Writing token: {e:#}"))?;
    if matches!(new_kind, AuthKind::Password) {
        state
            .keychain
            .put_credentials(
                id,
                input.username.as_deref().unwrap(),
                input.password.as_deref().unwrap(),
            )
            .map_err(|e| format!("Writing credentials: {e:#}"))?;
    }
    conn.auth_kind = new_kind;
    reg.upsert(conn);
    reg.save(&state.registry_path).map_err(|e| format!("{e:#}"))?;
    Ok(())
}

#[tauri::command]
pub async fn remove_connection(
    state: State<'_, AppState>,
    connection_id: String,
) -> Result<(), String> {
    let id: Ulid = connection_id.parse().map_err(|_| "Bad id".to_string())?;
    let mut reg = state.registry.lock().await;
    let conn = reg
        .remove(id)
        .ok_or_else(|| "Connection not found".to_string())?;
    state
        .keychain
        .delete_all(id)
        .map_err(|e| format!("Clearing Keychain: {e:#}"))?;
    crate::folder::trash_folder(&conn.folder)
        .map_err(|e| format!("Moving folder to Trash: {e:#}"))?;
    reg.save(&state.registry_path).map_err(|e| format!("{e:#}"))?;
    Ok(())
}

#[derive(Serialize)]
pub struct DiagnosticsResponse {
    pub app_version: String,
    pub rdc_version: String,
    pub os_version: String,
    pub connection_count: usize,
    pub log_lines: Vec<String>,
}

#[tauri::command]
pub async fn get_diagnostics(state: State<'_, AppState>) -> Result<DiagnosticsResponse, String> {
    let reg = state.registry.lock().await;
    Ok(DiagnosticsResponse {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        rdc_version: rdc_version_string(),
        os_version: os_version_string(),
        connection_count: reg.connections().len(),
        log_lines: state.diag.snapshot(),
    })
}

fn rdc_version_string() -> String {
    rdc::version().unwrap_or("unknown").to_string()
}

fn os_version_string() -> String {
    match std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
    {
        Ok(o) if o.status.success() => format!(
            "macOS {}",
            String::from_utf8_lossy(&o.stdout).trim()
        ),
        _ => "unknown".into(),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateSettingsInput {
    pub default_folder_parent: String,
    pub update_channel: String, // "stable" | "beta"
}

#[tauri::command]
pub async fn update_settings(
    state: State<'_, AppState>,
    input: UpdateSettingsInput,
) -> Result<(), String> {
    let mut s = state.settings.lock().await;
    s.default_folder_parent = std::path::PathBuf::from(&input.default_folder_parent);
    s.update_channel = match input.update_channel.as_str() {
        "stable" => crate::settings::UpdateChannel::Stable,
        "beta" => crate::settings::UpdateChannel::Beta,
        other => return Err(format!("Unknown channel '{other}'.")),
    };
    s.save(&state.settings_path).map_err(|e| format!("{e:#}"))?;
    Ok(())
}

/// Open `path` in the OS file manager (Finder on macOS). Replaces the
/// `__TAURI__.shell.open` global lookup, which isn't auto-exposed by
/// Tauri 2 even with `withGlobalTauri: true`.
#[tauri::command]
pub fn reveal_folder(path: String) -> Result<(), String> {
    crate::folder::reveal(std::path::Path::new(&path)).map_err(|e| format!("{e:#}"))
}
