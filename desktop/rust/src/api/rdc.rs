//! FRB bridge from the Flutter desktop app ("Rossum Local") into the rdc core.
//!
//! Exposes projects that group multiple environments (e.g. dev/test/prod),
//! each syncable independently, and re-uses rdc's own file/credential/sync
//! helpers — this crate adds no new credential or sync logic. Only the FFI
//! glue differs (StreamSink progress + anyhow errors).

use crate::discover::{self, AuthKindRaw, Project};
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
pub struct EnvSummary {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    pub last_sync_unix: Option<i64>,
    pub file_count: u64,
}

impl From<&crate::discover::EnvInfo> for EnvSummary {
    fn from(e: &crate::discover::EnvInfo) -> Self {
        Self {
            name: e.name.clone(),
            api_base: e.api_base.clone(),
            org_id: e.org_id,
            auth_kind: e.auth_kind.into(),
            last_sync_unix: e.last_sync_unix,
            file_count: e.file_count,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub folder: String,
    pub envs: Vec<EnvSummary>,
}

impl From<&Project> for ProjectSummary {
    fn from(p: &Project) -> Self {
        Self {
            id: p.id().to_string(),
            name: p.name().to_string(),
            folder: p.folder.display().to_string(),
            envs: p.envs.iter().map(EnvSummary::from).collect(),
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

/// List every Project under `parent`. Non-project folders are skipped.
pub fn list_projects(parent: String) -> Vec<ProjectSummary> {
    discover::scan(Path::new(&parent))
        .iter()
        .map(ProjectSummary::from)
        .collect()
}

// ---------------------------------------------------------------- add

/// Create a new Project: write `rdc.toml` + secrets for a first env named
/// `main` under a unique slug. (Additional envs are added in a later phase.)
pub fn add_project(parent: String, input: AddConnectionInput) -> Result<ProjectSummary> {
    let parent = PathBuf::from(parent);
    let used: HashSet<String> = discover::scan(&parent)
        .iter()
        .map(|p| p.name().to_string())
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
        "main",
        input.auth_kind,
        input.token.as_deref(),
        input.username.as_deref(),
        input.password.as_deref(),
    )?;

    discover::find(&parent, &slug)
        .as_ref()
        .map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("Project not found after add"))
}

// ---------------------------------------------------------------- validate

/// Validate that `path` is an rdc project (≥1 env, any names) and return its
/// summary. Does not move, copy, or symlink anything.
pub fn validate_existing_project(path: String) -> Result<ProjectSummary> {
    let source = PathBuf::from(&path);
    if !source.is_dir() {
        return Err(anyhow!("Not a folder: {path}"));
    }
    if !source.join("rdc.toml").exists() {
        return Err(anyhow!(
            "{path} doesn't look like an rdc project (no rdc.toml). Run `rdc init` there first."
        ));
    }
    discover::inspect(&source)
        .as_ref()
        .map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("{path} has no environments defined in rdc.toml."))
}

// ---------------------------------------------------------------- edit

/// Rename the project folder (from `input.name`) if it changed, and rewrite the
/// named `env`'s api_base/org_id through rdc's own config writer. Credentials
/// are only replaced when supplied (blank = keep existing).
pub fn edit_project(
    folder: String,
    env: String,
    input: EditConnectionInput,
) -> Result<ProjectSummary> {
    let mut folder = PathBuf::from(&folder);
    if !folder.join("rdc.toml").exists() {
        return Err(anyhow!("Project not found"));
    }
    let parent = folder
        .parent()
        .ok_or_else(|| anyhow!("Project has no parent folder"))?
        .to_path_buf();
    let current_slug = folder
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let used: HashSet<String> = discover::scan(&parent)
        .iter()
        .map(|p| p.name().to_string())
        .filter(|n| n != &current_slug)
        .collect();
    let desired_slug = rdc::slug::slugify_unique(&input.name, &used);
    if desired_slug != current_slug {
        let new_folder = parent.join(&desired_slug);
        if new_folder.exists() {
            return Err(anyhow!("A project named \"{}\" already exists here.", input.name));
        }
        std::fs::rename(&folder, &new_folder).map_err(|e| anyhow!("renaming the project: {e}"))?;
        folder = new_folder;
    }

    let toml_path = folder.join("rdc.toml");
    let mut cfg = rdc::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    let ec = cfg
        .envs
        .get_mut(&env)
        .ok_or_else(|| anyhow!("This project has no `{env}` environment."))?;
    ec.api_base = input.api_base.trim_end_matches('/').to_string();
    ec.org_id = input.org_id;
    cfg.save(&toml_path).map_err(|e| anyhow!("{e:#}"))?;

    let has_new_credentials = match input.auth_kind {
        AuthKind::Token => input.token.as_deref().is_some_and(|s| !s.is_empty()),
        AuthKind::Password => {
            input.username.as_deref().is_some_and(|s| !s.is_empty())
                || input.password.as_deref().is_some_and(|s| !s.is_empty())
        }
    };
    if has_new_credentials {
        let _ = std::fs::remove_file(folder.join(format!("secrets/{env}.secrets.json")));
        write_credentials(
            &folder,
            &env,
            input.auth_kind,
            input.token.as_deref(),
            input.username.as_deref(),
            input.password.as_deref(),
        )?;
    }

    discover::inspect(&folder)
        .as_ref()
        .map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("Project not found after edit"))
}

// ---------------------------------------------------------------- add/remove env

#[derive(Debug, Clone)]
pub struct AddEnvInput {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Add a new environment to an existing project. Errors if the env already exists.
pub fn add_env(folder: String, input: AddEnvInput) -> Result<ProjectSummary> {
    let folder = PathBuf::from(&folder);
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(anyhow!("Environment name is required."));
    }
    let toml_path = folder.join("rdc.toml");
    let mut cfg = rdc::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    if cfg.envs.contains_key(&name) {
        return Err(anyhow!("An environment named \"{name}\" already exists in this project."));
    }
    cfg.envs.insert(
        name.clone(),
        rdc::config::EnvConfig {
            api_base: input.api_base.trim_end_matches('/').to_string(),
            org_id: input.org_id,
        },
    );
    cfg.save(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    write_credentials(
        &folder, &name,
        input.auth_kind, input.token.as_deref(), input.username.as_deref(), input.password.as_deref(),
    )?;
    discover::inspect(&folder)
        .as_ref()
        .map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("Project not found after add_env"))
}

/// Remove an environment: its `rdc.toml` section, snapshot, secrets, and state.
/// If it was the last env, the whole project is trashed and `Ok(None)` returned.
pub fn remove_env(folder: String, env: String) -> Result<Option<ProjectSummary>> {
    let folder = PathBuf::from(&folder);
    let toml_path = folder.join("rdc.toml");
    let mut cfg = rdc::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    if !cfg.envs.contains_key(&env) {
        return Err(anyhow!("This project has no \"{env}\" environment."));
    }
    if cfg.envs.len() == 1 {
        // Last environment — removing it means removing the project.
        trash_project(folder.display().to_string())?;
        return Ok(None);
    }
    cfg.envs.remove(&env);
    cfg.save(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    // Best-effort cleanup of the env's on-disk artifacts.
    let _ = std::fs::remove_dir_all(folder.join(format!("envs/{env}")));
    let _ = std::fs::remove_file(folder.join(format!("secrets/{env}.secrets.json")));
    let _ = std::fs::remove_file(folder.join(format!(".rdc/state/{env}.lock.json")));
    let _ = std::fs::remove_file(folder.join(format!(".rdc/state/{env}.lock")));
    let _ = std::fs::remove_dir_all(folder.join(format!(".rdc/state/{env}.base")));
    discover::inspect(&folder)
        .as_ref()
        .map(ProjectSummary::from)
        .map(Some)
        .ok_or_else(|| anyhow!("Project not found after remove_env"))
}

// ---------------------------------------------------------------- sync

/// Pull-only sync of one environment. Scaffolds init files, resolves the token
/// (silent re-login in password mode), then runs `sync_no_push`. Progress is
/// streamed as `SyncPhase`.
///
/// Returns `Ok(())` even when the sync itself fails — the terminal outcome
/// (success or error) is conveyed to the caller via the `SyncPhase::Done` /
/// `SyncPhase::Error` stream events, not via this function's `Result`.
pub fn sync_env(
    folder: String,
    env: String,
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
        rdc::cli::init::write_scaffold_files(&folder, &env, &api_base, org_id)?;
        let token = rdc::secrets::resolve_token(&folder, &env, &api_base).await?;
        rdc::cli::sync::embed::sync_no_push_logged(&folder, &env, &token, Box::new(forwarder))
            .await?;
        Ok(discover::count_files(&folder.join(format!("envs/{env}"))))
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

/// Move a managed Project's folder to the OS trash/recycle bin.
pub fn trash_project(folder: String) -> Result<()> {
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
    env: &str,
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
            rdc::secrets::write_secrets_file(folder, env, t, None).map_err(|e| anyhow!("{e:#}"))?;
        }
        AuthKind::Password => {
            let u = username
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("Username is required."))?;
            let p = password
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("Password is required."))?;
            rdc::secrets::save_password_credentials(folder, env, u, p)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_summary_from_multi_env_project() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("acme");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("rdc.toml"),
            "[envs.dev]\napi_base = \"https://d.test/api/v1\"\norg_id = 1\n\
             [envs.prod]\napi_base = \"https://p.test/api/v1\"\norg_id = 2\n",
        )
        .unwrap();
        let p = discover::inspect(&folder).unwrap();
        let summary = ProjectSummary::from(&p);
        assert_eq!(summary.name, "acme");
        assert_eq!(summary.envs.len(), 2);
        assert_eq!(summary.envs[0].name, "dev");
        assert_eq!(summary.envs[1].org_id, 2);
    }

    fn seed_project(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let folder = dir.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("rdc.toml"),
            "[envs.main]\napi_base = \"https://m.test/api/v1\"\norg_id = 1\n").unwrap();
        rdc::secrets::write_secrets_file(&folder, "main", "tok", None).unwrap();
        folder
    }

    #[test]
    fn add_env_appends_a_second_env() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let p = add_env(folder.display().to_string(), AddEnvInput {
            name: "prod".into(), api_base: "https://p.test/api/v1/".into(), org_id: 2,
            auth_kind: AuthKind::Token, token: Some("tok2".into()), username: None, password: None,
        }).unwrap();
        assert_eq!(p.envs.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), vec!["main", "prod"]);
        let prod = p.envs.iter().find(|e| e.name == "prod").unwrap();
        assert_eq!(prod.api_base, "https://p.test/api/v1"); // trailing slash trimmed
        assert!(folder.join("secrets/prod.secrets.json").exists());
    }

    #[test]
    fn add_env_rejects_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let err = add_env(folder.display().to_string(), AddEnvInput {
            name: "main".into(), api_base: "https://x.test/api/v1".into(), org_id: 9,
            auth_kind: AuthKind::Token, token: Some("t".into()), username: None, password: None,
        }).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"));
    }

    #[test]
    fn remove_env_drops_one_and_keeps_project() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        add_env(folder.display().to_string(), AddEnvInput {
            name: "prod".into(), api_base: "https://p.test/api/v1".into(), org_id: 2,
            auth_kind: AuthKind::Token, token: Some("t2".into()), username: None, password: None,
        }).unwrap();
        let p = remove_env(folder.display().to_string(), "prod".into()).unwrap().unwrap();
        assert_eq!(p.envs.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), vec!["main"]);
        assert!(!folder.join("secrets/prod.secrets.json").exists());
    }

    #[test]
    fn remove_last_env_trashes_project_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let r = remove_env(folder.display().to_string(), "main".into()).unwrap();
        assert!(r.is_none());
        assert!(!folder.exists()); // whole project gone (moved to trash)
    }
}
