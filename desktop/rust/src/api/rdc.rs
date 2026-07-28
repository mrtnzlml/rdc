//! FRB bridge from the Flutter desktop app ("Rossum Local") into the rdc core.
//!
//! Mirrors the retired `rdc-ffi` (UniFFI) surface 1:1 and re-uses rdc's own
//! file/credential/sync helpers — this crate adds no new credential or sync
//! logic. Only the FFI glue differs (StreamSink progress + anyhow errors).

use crate::discover::{self, AuthKindRaw, Connection};
use anyhow::{anyhow, Result};
use crate::frb_generated::StreamSink;
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};

/// Runs once when the Dart side calls `RustLib.init()`.
#[flutter_rust_bridge::frb(init)]
pub fn init_app() {
    flutter_rust_bridge::setup_default_user_utils();
}

// ---------------------------------------------------------------- types

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    Token,
    Password,
}

impl From<AuthKindRaw> for AuthKind {
    fn from(r: AuthKindRaw) -> Self {
        match r {
            AuthKindRaw::Token => AuthKind::Token,
            AuthKindRaw::Password => AuthKind::Password,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectionSummary {
    pub id: String,
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub folder: String,
    pub auth_kind: AuthKind,
    pub last_sync_unix: Option<i64>,
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
            auth_kind: c.auth_kind.into(),
            last_sync_unix: c.last_sync_unix,
            file_count: c.file_count,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AddConnectionInput {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EditConnectionInput {
    /// The connection name; if it changes, the folder is renamed.
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    /// Credentials are optional on edit: leave all blank to keep the existing
    /// ones. Provide new values (matching `auth_kind`) to replace them.
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SyncPhase {
    Started,
    /// One line of rdc's real, rendered sync log (plain text, no color).
    Log { line: String },
    Done { file_count: u64 },
    Error { message: String },
}

// ---------------------------------------------------------------- version

/// rdc's package version, surfaced to the app's About box.
pub fn rdc_version() -> Option<String> {
    rdc::version().map(|s| s.to_string())
}

// ---------------------------------------------------------------- list

/// List every Connection under `parent`. Non-project folders are skipped.
pub fn list_connections(parent: String) -> Vec<ConnectionSummary> {
    discover::scan(Path::new(&parent))
        .iter()
        .map(ConnectionSummary::from)
        .collect()
}

// ---------------------------------------------------------------- add

/// Create a new Connection: write `rdc.toml` + secrets under a unique slug.
pub fn add_connection(parent: String, input: AddConnectionInput) -> Result<ConnectionSummary> {
    let parent = PathBuf::from(parent);
    let used: HashSet<String> = discover::scan(&parent)
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    let slug = rdc::slug::slugify_unique(&input.name, &used);
    let folder = parent.join(&slug);
    std::fs::create_dir_all(&folder).map_err(|e| anyhow!("creating folder: {e}"))?;

    let api_base = input.api_base.trim_end_matches('/').to_string();
    let rdc_toml = format!(
        "[envs.main]\napi_base = \"{api_base}\"\norg_id = {}\n",
        input.org_id
    );
    std::fs::write(folder.join("rdc.toml"), rdc_toml)
        .map_err(|e| anyhow!("writing rdc.toml: {e}"))?;

    write_credentials(
        &folder,
        input.auth_kind,
        input.token.as_deref(),
        input.username.as_deref(),
        input.password.as_deref(),
    )?;

    discover::find(&parent, &slug)
        .as_ref()
        .map(ConnectionSummary::from)
        .ok_or_else(|| anyhow!("Connection not found after add"))
}

// ---------------------------------------------------------------- validate

/// Validate that `path` is a single-env (`main`) rdc project and return its
/// summary. Does not move, copy, or symlink anything.
pub fn validate_existing_project(path: String) -> Result<ConnectionSummary> {
    let source = PathBuf::from(&path);
    if !source.is_dir() {
        return Err(anyhow!("Not a folder: {path}"));
    }
    let rdc_toml = source.join("rdc.toml");
    if !rdc_toml.exists() {
        return Err(anyhow!(
            "{path} doesn't look like an rdc project (no rdc.toml). Run `rdc init` there first."
        ));
    }
    let body = std::fs::read_to_string(&rdc_toml).map_err(|e| anyhow!("reading rdc.toml: {e}"))?;
    if !body.contains("[envs.main]") {
        return Err(anyhow!(
            "{} has no [envs.main] section; only single-env projects named `main` are supported.",
            rdc_toml.display()
        ));
    }
    discover::inspect(&source)
        .as_ref()
        .map(ConnectionSummary::from)
        .ok_or_else(|| anyhow!("Project not discoverable"))
}

// ---------------------------------------------------------------- edit

/// Update a Connection's settings, renaming its folder if the name changed.
/// `api_base`/`org_id` are rewritten through rdc's own config writer so the
/// file matches the CLI's format exactly. Credentials are only touched when
/// new ones are supplied (blank = keep existing). Returns the (possibly moved)
/// Connection so the caller can reselect it.
pub fn edit_connection(folder: String, input: EditConnectionInput) -> Result<ConnectionSummary> {
    let mut folder = PathBuf::from(&folder);
    if !folder.join("rdc.toml").exists() {
        return Err(anyhow!("Connection not found"));
    }

    // Rename the folder when the name (slug) changed.
    let parent = folder
        .parent()
        .ok_or_else(|| anyhow!("Connection has no parent folder"))?
        .to_path_buf();
    let current_slug = folder
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let used: HashSet<String> = discover::scan(&parent)
        .iter()
        .map(|c| c.name().to_string())
        .filter(|n| n != &current_slug)
        .collect();
    let desired_slug = rdc::slug::slugify_unique(&input.name, &used);
    if desired_slug != current_slug {
        let new_folder = parent.join(&desired_slug);
        if new_folder.exists() {
            return Err(anyhow!(
                "A connection named \"{}\" already exists here.",
                input.name
            ));
        }
        std::fs::rename(&folder, &new_folder).map_err(|e| anyhow!("renaming the connection: {e}"))?;
        folder = new_folder;
    }

    // Rewrite api_base/org_id through rdc's own config (canonical + lossless).
    let toml_path = folder.join("rdc.toml");
    let mut cfg = rdc::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    let env = cfg
        .envs
        .get_mut("main")
        .ok_or_else(|| anyhow!("This project has no `main` environment."))?;
    env.api_base = input.api_base.trim_end_matches('/').to_string();
    env.org_id = input.org_id;
    cfg.save(&toml_path).map_err(|e| anyhow!("{e:#}"))?;

    // Only replace credentials if new ones were supplied.
    let has_new_credentials = match input.auth_kind {
        AuthKind::Token => input.token.as_deref().is_some_and(|s| !s.is_empty()),
        AuthKind::Password => {
            input.username.as_deref().is_some_and(|s| !s.is_empty())
                || input.password.as_deref().is_some_and(|s| !s.is_empty())
        }
    };
    if has_new_credentials {
        let _ = std::fs::remove_file(folder.join("secrets/main.secrets.json"));
        write_credentials(
            &folder,
            input.auth_kind,
            input.token.as_deref(),
            input.username.as_deref(),
            input.password.as_deref(),
        )?;
    }

    discover::inspect(&folder)
        .as_ref()
        .map(ConnectionSummary::from)
        .ok_or_else(|| anyhow!("Connection not found after edit"))
}

// ---------------------------------------------------------------- sync

/// Pull-only sync of one Connection. Scaffolds init files, resolves the token
/// (silent re-login in password mode), then runs `sync_no_push`. Progress is
/// streamed as `SyncPhase`; the stream conveys the terminal outcome (Done or
/// Error) rather than throwing.
pub fn sync_connection(
    folder: String,
    api_base: String,
    org_id: u64,
    sink: StreamSink<SyncPhase>,
) -> Result<()> {
    let folder = PathBuf::from(folder);
    let _ = sink.add(SyncPhase::Started);

    // Forward rdc's real, rendered log into the stream, line by line.
    let forwarder = LineForwarder {
        sink: sink.clone(),
        buf: Vec::new(),
    };
    let result: Result<u64> = block_on(async {
        rdc::cli::init::write_scaffold_files(&folder, "main", &api_base, org_id)?;
        let token = rdc::secrets::resolve_token(&folder, "main", &api_base).await?;
        rdc::cli::sync::embed::sync_no_push_logged(&folder, "main", &token, Box::new(forwarder))
            .await?;
        Ok(discover::count_files(&folder.join("envs/main")))
    });

    match result {
        Ok(file_count) => {
            // run_cycle omits its closing summary when a renderer is supplied,
            // so add our own completion line before the terminal Done phase.
            let _ = sink.add(SyncPhase::Log {
                line: format!("✓ done · {file_count} files"),
            });
            let _ = sink.add(SyncPhase::Done { file_count });
        }
        Err(e) => {
            let _ = sink.add(SyncPhase::Error {
                message: format!("{e:#}"),
            });
        }
    }
    Ok(())
}

/// A `std::io::Write` that splits rdc's rendered log output into whole lines
/// and forwards each as `SyncPhase::Log`. rdc writes one newline-terminated
/// line per event (plain text under `ColorMode::Plain`, non-TTY).
struct LineForwarder {
    sink: StreamSink<SyncPhase>,
    buf: Vec<u8>,
}

impl std::io::Write for LineForwarder {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let text = String::from_utf8_lossy(&line)
                .trim_end_matches(['\n', '\r'])
                .to_string();
            if !text.is_empty() {
                let _ = self.sink.add(SyncPhase::Log { line: text });
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------- trash / reveal

/// Move a managed Connection's folder to the OS trash/recycle bin.
pub fn trash_connection(folder: String) -> Result<()> {
    let path = PathBuf::from(&folder);
    #[cfg(target_os = "macos")]
    {
        // Use NSFileManager rather than the default Finder/AppleScript backend,
        // which requires Apple Events ("Automation") permission the app does not
        // have (error -1743). NSFileManager needs no such permission.
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        let mut ctx = trash::TrashContext::default();
        ctx.set_delete_method(DeleteMethod::NsFileManager);
        ctx.delete(&path)
            .map_err(|e| anyhow!("Couldn't move the folder to the Trash: {e}"))?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        trash::delete(&path).map_err(|e| anyhow!("Couldn't move the folder to the Trash: {e}"))?;
    }
    Ok(())
}

/// Reveal a path in the OS file manager (Finder / Explorer / file manager).
pub fn reveal_in_file_manager(path: String) -> Result<()> {
    let p = PathBuf::from(&path);
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&p)
            .spawn()
            .map_err(|e| anyhow!("open -R: {e}"))?;
    }
    #[cfg(target_os = "windows")]
    {
        // explorer returns a non-zero exit even on success, so don't check it.
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", p.display()))
            .spawn()
            .map_err(|e| anyhow!("explorer /select: {e}"))?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let uri = format!("file://{}", p.display());
        let dbus = std::process::Command::new("dbus-send")
            .args([
                "--session",
                "--dest=org.freedesktop.FileManager1",
                "--type=method_call",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
            ])
            .arg(format!("array:string:{uri}"))
            .arg("string:")
            .spawn();
        if dbus.is_err() {
            let dir = p.parent().unwrap_or(&p);
            std::process::Command::new("xdg-open")
                .arg(dir)
                .spawn()
                .map_err(|e| anyhow!("xdg-open: {e}"))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- internal

/// Write credentials via rdc's own helpers. Empty strings are rejected the
/// same way the original bridge did.
fn write_credentials(
    folder: &Path,
    auth: AuthKind,
    token: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<()> {
    match auth {
        AuthKind::Token => {
            let t = token
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("Token is required."))?;
            rdc::secrets::write_secrets_file(folder, "main", t, None)
                .map_err(|e| anyhow!("{e:#}"))?;
        }
        AuthKind::Password => {
            let u = username
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("Username is required."))?;
            let p = password
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("Password is required."))?;
            rdc::secrets::save_password_credentials(folder, "main", u, p)
                .map_err(|e| anyhow!("{e:#}"))?;
        }
    }
    Ok(())
}

/// Block the calling thread on `fut` using a current-thread runtime (the sync
/// engine holds `!Send` types across awaits, so it cannot run multi-threaded).
fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread tokio runtime")
        .block_on(fut)
}
