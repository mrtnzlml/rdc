//! Tauri command surface — the only thing the WebView frontend calls.
//!
//! Every Connection-mutating command boils down to writing rdc's
//! standard on-disk artifacts (`rdc.toml`, `secrets/main.secrets.json`)
//! and then either listing folders or invoking `sync::run_sync`. No
//! registry, no Keychain, no app-private credential store.

use crate::discover::{self, AuthKind, Connection};
use crate::state::AppState;
use crate::{folder, sync};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

#[derive(Serialize)]
pub struct ConnectionSummary {
    pub id: String,
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub folder: String,
    pub auth_kind: AuthKind,
    pub last_sync_unix: Option<i64>,
    pub last_status: String,
    pub last_status_message: Option<String>,
    pub file_count: u64,
}

impl From<&Connection> for ConnectionSummary {
    fn from(c: &Connection) -> Self {
        Self {
            id: c.id().to_string(),
            name: c.name().to_string(),
            api_base: c.api_base.clone(),
            org_id: c.org_id,
            folder: c.folder.display().to_string(),
            auth_kind: c.auth_kind,
            last_sync_unix: c.last_sync_unix,
            last_status: if c.last_sync_unix.is_some() { "ok".into() } else { "never".into() },
            last_status_message: None,
            file_count: c.file_count,
        }
    }
}

#[tauri::command]
pub fn list_connections(state: State<'_, AppState>) -> Vec<ConnectionSummary> {
    discover::scan(&state.parent)
        .iter()
        .map(ConnectionSummary::from)
        .collect()
}

#[derive(Deserialize)]
pub struct AddConnectionInput {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: String, // "token" | "password"
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[tauri::command]
pub fn add_connection(
    state: State<'_, AppState>,
    input: AddConnectionInput,
) -> Result<ConnectionSummary, String> {
    let used = discover::scan(&state.parent)
        .into_iter()
        .map(|c| c.name().to_string())
        .collect::<std::collections::HashSet<_>>();
    let slug = rdc::slug::slugify_unique(&input.name, &used);
    let folder = state.parent.join(&slug);
    std::fs::create_dir_all(&folder).map_err(|e| format!("creating folder: {e}"))?;

    let api_base = input.api_base.trim_end_matches('/').to_string();
    let rdc_toml = format!("[envs.main]\napi_base = \"{api_base}\"\norg_id = {}\n", input.org_id);
    std::fs::write(folder.join("rdc.toml"), rdc_toml)
        .map_err(|e| format!("writing rdc.toml: {e}"))?;

    write_credentials(&folder, &input.auth_kind, input.token.as_deref(), input.username.as_deref(), input.password.as_deref())?;

    discover::find(&state.parent, &slug)
        .as_ref()
        .map(ConnectionSummary::from)
        .ok_or_else(|| "Connection not found after add".into())
}

/// Attach an existing rdc project (e.g. one created via `rdc init` in
/// the CLI) by placing a symlink to it inside the discovery directory.
/// The scanner then picks it up like any other Connection. The user's
/// original folder is never moved or copied.
#[tauri::command]
pub fn open_existing_project(
    state: State<'_, AppState>,
    path: String,
) -> Result<ConnectionSummary, String> {
    let source = std::path::PathBuf::from(&path);
    if !source.is_dir() {
        return Err(format!("Not a folder: {}", source.display()));
    }
    let rdc_toml = source.join("rdc.toml");
    if !rdc_toml.exists() {
        return Err(format!(
            "{} doesn't look like an rdc project (no rdc.toml). Run `rdc init` there first.",
            source.display()
        ));
    }
    // Confirm it has a `[envs.main]` env — the only shape the desktop
    // app understands. Multi-env projects from the CLI use other env
    // names; we leave those to the CLI.
    let body = std::fs::read_to_string(&rdc_toml).map_err(|e| format!("reading rdc.toml: {e}"))?;
    if !body.contains("[envs.main]") {
        return Err(format!(
            "{} has no [envs.main] section; only single-env projects named `main` are supported.",
            rdc_toml.display()
        ));
    }

    let name = source
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("Folder name is not valid UTF-8")?;
    let link = state.parent.join(name);
    if link.exists() {
        // If the existing entry already points at the same source, treat
        // this as a no-op success — let the user re-open without error.
        if let Ok(target) = std::fs::read_link(&link) {
            if target == source {
                return discover::find(&state.parent, name)
                    .as_ref()
                    .map(ConnectionSummary::from)
                    .ok_or_else(|| "Existing entry but not discoverable".into());
            }
        }
        return Err(format!(
            "A Connection named '{name}' already exists. Rename the source folder or remove the existing Connection first."
        ));
    }
    std::os::unix::fs::symlink(&source, &link).map_err(|e| format!("creating symlink: {e}"))?;
    discover::find(&state.parent, name)
        .as_ref()
        .map(ConnectionSummary::from)
        .ok_or_else(|| "Symlinked, but not discoverable afterwards".into())
}

#[derive(Serialize, Clone)]
pub struct SyncProgress {
    pub connection_id: String,
    pub phase: String,
    pub message: Option<String>,
    pub file_count: Option<u64>,
}

#[tauri::command]
pub async fn sync_connection(
    app: AppHandle,
    state: State<'_, AppState>,
    connection_id: String,
) -> Result<(), String> {
    let Some(conn) = discover::find(&state.parent, &connection_id) else {
        return Err("Connection not found".into());
    };
    let id = conn.id().to_string();

    let _ = app.emit(
        "sync-progress",
        SyncProgress {
            connection_id: id.clone(),
            phase: "started".into(),
            message: None,
            file_count: None,
        },
    );

    let app2 = app.clone();
    let folder = conn.folder.clone();
    let api_base = conn.api_base.clone();
    let org_id = conn.org_id;
    // rdc's sync graph uses `!Send` types (e.g. StdinLock held across
    // await points) so we can't await it directly on the Tauri runtime
    // worker thread. Spawn a single-threaded runtime on a blocking
    // pool thread and run it there.
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build rt");
        let outcome = rt.block_on(sync::run_sync(&folder, &api_base, org_id));
        let progress = match outcome {
            Ok(file_count) => SyncProgress {
                connection_id: id,
                phase: "done".into(),
                message: None,
                file_count: Some(file_count),
            },
            Err(e) => SyncProgress {
                connection_id: id,
                phase: "error".into(),
                message: Some(format!("{e:#}")),
                file_count: None,
            },
        };
        let _ = app2.emit("sync-progress", progress);
    });

    // Sync lock isn't strictly enforced here in v0.1 — rdc's env lock
    // inside `sync_no_push` already prevents concurrent writes to the
    // same Connection folder. Cross-Connection serialization could be
    // added by acquiring `state.sync_lock` inside the spawned task if
    // contention ever surfaces.
    let _ = &state.sync_lock;
    Ok(())
}

#[derive(Deserialize)]
pub struct EditCredentialsInput {
    pub connection_id: String,
    pub auth_kind: String,
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[tauri::command]
pub fn edit_credentials(
    state: State<'_, AppState>,
    input: EditCredentialsInput,
) -> Result<(), String> {
    let Some(conn) = discover::find(&state.parent, &input.connection_id) else {
        return Err("Connection not found".into());
    };
    // Wipe existing credential state before writing fresh fields. The
    // user may be flipping from password mode → token mode (or vice
    // versa); leaving stale fields could let rdc try the wrong auth path.
    let _ = std::fs::remove_file(conn.folder.join("secrets/main.secrets.json"));
    write_credentials(
        &conn.folder,
        &input.auth_kind,
        input.token.as_deref(),
        input.username.as_deref(),
        input.password.as_deref(),
    )
}

#[tauri::command]
pub fn remove_connection(
    state: State<'_, AppState>,
    connection_id: String,
) -> Result<(), String> {
    let folder = state.parent.join(&connection_id);
    if !folder.exists() {
        return Err("Connection not found".into());
    }
    folder::trash_folder(&folder).map_err(|e| format!("Moving to Trash: {e:#}"))
}

#[tauri::command]
pub fn reveal_folder(path: String) -> Result<(), String> {
    folder::reveal(std::path::Path::new(&path)).map_err(|e| format!("{e:#}"))
}

fn write_credentials(
    folder: &std::path::Path,
    kind: &str,
    token: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<(), String> {
    match kind {
        "token" => {
            let t = token.ok_or("Token is required.")?;
            if t.is_empty() {
                return Err("Token is required.".into());
            }
            rdc::secrets::write_secrets_file(folder, "main", t, None)
                .map_err(|e| format!("writing token: {e:#}"))?;
        }
        "password" => {
            let u = username.ok_or("Username is required.")?;
            let p = password.ok_or("Password is required.")?;
            if u.is_empty() || p.is_empty() {
                return Err("Username and password are required.".into());
            }
            rdc::secrets::save_password_credentials(folder, "main", u, p)
                .map_err(|e| format!("writing credentials: {e:#}"))?;
        }
        other => return Err(format!("Unknown auth_kind '{other}'.")),
    }
    Ok(())
}

