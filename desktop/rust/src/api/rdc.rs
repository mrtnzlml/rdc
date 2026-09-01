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

/// One answerable choice, as offered to the UI. `key` is a String rather
/// than a char because FRB has no char; it is always exactly one character.
#[derive(Debug, Clone)]
pub struct PromptChoice {
    pub key: String,
    pub label: String,
}

/// Mirrors `rdc::cli::stdin_coord::PromptKind` across the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKindDto {
    Conflict,
    RemoteDelete,
    PushDrift,
    BulkConfirm,
    DeleteGate,
    DeleteDrift,
    MdhIndexDrop,
    MdhRowDelete,
    /// A coordinated read whose site never announced. Should be unreachable;
    /// it exists so that if it ever happens the UI can say so instead of
    /// silently mislabelling the prompt as a conflict. Render it as an
    /// explicit "unrecognised prompt" state, not as a normal dialog.
    Unknown,
}

#[derive(Debug, Clone)]
pub enum SyncPhase {
    Started,
    /// One line of rdc's real, rendered sync log (plain text, no color).
    Log { line: String },
    /// A cycle is blocked waiting for an answer. Reply with `answer_prompt`
    /// using this `id`. The diff/list this refers to has already arrived as
    /// `Log` lines.
    Prompt {
        id: u64,
        kind: PromptKindDto,
        question: String,
        keys: Vec<PromptChoice>,
    },
    /// The prompt with this id no longer needs an answer (the watch stopped,
    /// or the cycle was torn down). Close the dialog.
    PromptResolved { id: u64 },
    /// A watch is between cycles. `next_poll_secs` is None when polling is
    /// disabled. rdc's own countdown never reaches an embedder — its
    /// in-place status line is a no-op off a TTY — so the app draws its own.
    Idle { next_poll_secs: Option<u64> },
    Done { file_count: u64 },
    Error { message: String },
    Stopped,
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

/// Create a new Project: write `rdc.toml` + secrets for a first env named by
/// the caller (`first_env.name`) under a unique slug. (Additional envs are
/// added later via `add_env`.)
pub fn add_project(parent: String, project_name: String, first_env: AddEnvInput) -> Result<ProjectSummary> {
    let parent = PathBuf::from(parent);
    let env_name = first_env.name.trim().to_string();
    if !valid_env_name(&env_name) {
        return Err(anyhow!("Environment name may only contain letters, digits, - and _ (and can't be empty)."));
    }
    let used: HashSet<String> = discover::scan(&parent).iter().map(|p| p.name().to_string()).collect();
    let slug = rdc::slug::slugify_unique(&project_name, &used);
    let folder = parent.join(&slug);
    std::fs::create_dir_all(&folder).map_err(|e| anyhow!("creating folder: {e}"))?;
    // credentials first (validates + writes secrets) so a bad credential never leaves a config behind
    write_credentials(&folder, &env_name, first_env.auth_kind,
        first_env.token.as_deref(), first_env.username.as_deref(), first_env.password.as_deref())?;
    let api_base = first_env.api_base.trim_end_matches('/').to_string();
    let rdc_toml = format!("[envs.{env_name}]\napi_base = \"{api_base}\"\norg_id = {}\n", first_env.org_id);
    std::fs::write(folder.join("rdc.toml"), rdc_toml).map_err(|e| anyhow!("writing rdc.toml: {e}"))?;
    discover::find(&parent, &slug).as_ref().map(ProjectSummary::from)
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

/// An env name may only contain letters, digits, `-` and `_` — the same rule
/// `rdc init`'s own prompt validator enforces (see `cli::init::prompt_env_name`).
/// Rejecting anything else here (path separators, `..`, etc.) before an env
/// name is ever interpolated into a filesystem path is what keeps `remove_env`
/// from being tricked into deleting outside the project folder.
fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

const INVALID_ENV_NAME_MSG: &str = "Environment name may only contain letters, digits, - and _.";

/// Add a new environment to an existing project. Errors if the env already exists.
pub fn add_env(folder: String, input: AddEnvInput) -> Result<ProjectSummary> {
    let folder = PathBuf::from(&folder);
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(anyhow!("Environment name is required."));
    }
    if !valid_env_name(&name) {
        return Err(anyhow!(INVALID_ENV_NAME_MSG));
    }
    let toml_path = folder.join("rdc.toml");
    let mut cfg = rdc::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    if cfg.envs.contains_key(&name) {
        return Err(anyhow!("An environment named \"{name}\" already exists in this project."));
    }
    // Validate + write credentials FIRST: if they're invalid, `rdc.toml` must
    // stay untouched so the caller can fix the input and retry `add_env`
    // without first having to remove a half-registered env.
    write_credentials(
        &folder, &name,
        input.auth_kind, input.token.as_deref(), input.username.as_deref(), input.password.as_deref(),
    )?;
    cfg.envs.insert(
        name.clone(),
        rdc::config::EnvConfig {
            api_base: input.api_base.trim_end_matches('/').to_string(),
            org_id: input.org_id,
        },
    );
    cfg.save(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    discover::inspect(&folder)
        .as_ref()
        .map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("Project not found after add_env"))
}

/// Remove an environment: its `rdc.toml` section, snapshot, secrets, and state.
/// If it was the last env, the whole project is trashed and `Ok(None)` returned.
pub fn remove_env(folder: String, env: String) -> Result<Option<ProjectSummary>> {
    // Defensive: guard against a malformed `rdc.toml` env key (or any other
    // caller mistake) before `env` is ever interpolated into a path below —
    // an env name with `..` or a path separator must never reach
    // `remove_dir_all`/`remove_file`.
    if !valid_env_name(&env) {
        return Err(anyhow!(INVALID_ENV_NAME_MSG));
    }
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
    // Best-effort cleanup of the env's on-disk artifacts, all routed through
    // rdc's own `Paths` so the layout stays single-sourced (src/paths.rs).
    let paths = rdc::paths::Paths::for_env(&folder, &env);
    let _ = std::fs::remove_dir_all(paths.env_root());
    let _ = std::fs::remove_file(paths.secrets_file());
    let _ = std::fs::remove_file(paths.lockfile());
    let _ = std::fs::remove_file(paths.env_lock());
    let _ = std::fs::remove_dir_all(paths.base_cache_root());
    // `Paths` has no bare "conflicts dir" accessor, only the per-file
    // `conflict_shadow_path`; passing the env root itself as `local_path`
    // makes it strip to an empty relpath, yielding the env's whole
    // `.rdc/conflicts/<env>/` shadow directory without hand-rolling the path.
    let _ = std::fs::remove_dir_all(paths.conflict_shadow_path(&paths.env_root()));
    discover::inspect(&folder)
        .as_ref()
        .map(ProjectSummary::from)
        .map(Some)
        .ok_or_else(|| anyhow!("Project not found after remove_env"))
}

/// Rename an environment `old` → `new` entirely locally: move every per-env
/// path, rewrite `.rdc/mapping.toml`, and rename the `[envs.<old>]` section.
///
/// Not fully transactional: the state/conflicts moves below stay best-effort
/// (as before), but once the substantive moves (`env_root`, `secrets`) have
/// happened, a later failure (mapping.toml rewrite, final `rdc.toml` save)
/// triggers a best-effort rollback of exactly those substantive moves before
/// the error is returned, so the registry and filesystem don't end up
/// disagreeing (`rdc.toml` still naming `old` while the files live under
/// `new`, or vice versa).
pub fn rename_env(folder: String, old: String, new: String) -> Result<ProjectSummary> {
    let folder = PathBuf::from(&folder);
    // Defensive, mirroring `remove_env`: `old` comes from a hand-editable
    // `rdc.toml` (via `validate_existing_project`, which adopts any env
    // name), so it must be checked before it's ever interpolated into a
    // filesystem path below — an env name with `..` or a path separator
    // must never reach `std::fs::rename`.
    if !valid_env_name(&old) {
        return Err(anyhow!(INVALID_ENV_NAME_MSG));
    }
    let new = new.trim().to_string();
    if !valid_env_name(&new) {
        return Err(anyhow!("Environment name may only contain letters, digits, - and _."));
    }
    let toml_path = folder.join("rdc.toml");
    let mut cfg = rdc::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    if !cfg.envs.contains_key(&old) {
        return Err(anyhow!("This project has no \"{old}\" environment."));
    }
    if new == old {
        return discover::inspect(&folder).as_ref().map(ProjectSummary::from)
            .ok_or_else(|| anyhow!("Project not found"));
    }
    if cfg.envs.contains_key(&new) {
        return Err(anyhow!("An environment named \"{new}\" already exists in this project."));
    }
    let op = rdc::paths::Paths::for_env(&folder, &old);
    let np = rdc::paths::Paths::for_env(&folder, &new);
    // 1. filesystem moves — env_root + secrets are the substantive ones; state/conflicts best-effort.
    let mut moved_env_root = false;
    let mut moved_secrets = false;
    // Best-effort undo of every per-env path this function may have moved —
    // used by every fallible step below that runs AFTER the moves. Covers
    // both the two substantive moves (env_root/secrets, gated on the tracked
    // flags since a hard failure partway through the forward move may leave
    // them untouched) AND the four best-effort cache/derived paths
    // (lockfile, env_lock, base_cache_root, conflicts dir) that are moved
    // unconditionally further down, before this closure is ever invoked —
    // reversing those is unconditional too; each `np...exists()` check
    // makes a given rename a no-op if that particular cache never existed.
    let rollback = |moved_env_root: bool, moved_secrets: bool| {
        for (n, o) in [
            (np.lockfile(), op.lockfile()),
            (np.env_lock(), op.env_lock()),
            (np.base_cache_root(), op.base_cache_root()),
        ] {
            if n.exists() {
                let _ = std::fs::rename(&n, &o);
            }
        }
        let new_conflicts = np.conflict_shadow_path(&np.env_root());
        let old_conflicts = op.conflict_shadow_path(&op.env_root());
        if new_conflicts.exists() {
            let _ = std::fs::rename(&new_conflicts, &old_conflicts);
        }
        if moved_secrets && np.secrets_file().exists() {
            let _ = std::fs::rename(np.secrets_file(), op.secrets_file());
        }
        if moved_env_root && np.env_root().exists() {
            let _ = std::fs::rename(np.env_root(), op.env_root());
        }
    };
    if op.env_root().exists() {
        std::fs::rename(op.env_root(), np.env_root())
            .map_err(|e| anyhow!("moving envs/{old} → envs/{new}: {e}"))?;
        moved_env_root = true;
    }
    if op.secrets_file().exists() {
        if let Some(parent) = np.secrets_file().parent() { let _ = std::fs::create_dir_all(parent); }
        if let Err(e) = std::fs::rename(op.secrets_file(), np.secrets_file()) {
            rollback(moved_env_root, moved_secrets);
            return Err(anyhow!("moving secrets: {e}"));
        }
        moved_secrets = true;
    }
    for (o, n) in [
        (op.lockfile(), np.lockfile()),
        (op.env_lock(), np.env_lock()),
        (op.base_cache_root(), np.base_cache_root()),
    ] {
        if o.exists() { let _ = std::fs::rename(&o, &n); }
    }
    let old_conflicts = op.conflict_shadow_path(&op.env_root());
    let new_conflicts = np.conflict_shadow_path(&np.env_root());
    if old_conflicts.exists() { let _ = std::fs::rename(&old_conflicts, &new_conflicts); }
    // 2. rewrite mapping.toml
    let mapping_path = op.mapping_file();
    if mapping_path.exists() {
        let mut g = match rdc::mapping::GenericMapping::load(&mapping_path) {
            Ok(g) => g,
            Err(e) => {
                rollback(moved_env_root, moved_secrets);
                return Err(anyhow!("{e:#}"));
            }
        };
        g.rename_env(&old, &new);
        if let Err(e) = g.save(&mapping_path) {
            rollback(moved_env_root, moved_secrets);
            return Err(anyhow!("{e:#}"));
        }
    }
    // 3. rename the rdc.toml section last (authoritative record)
    if let Some(env_cfg) = cfg.envs.remove(&old) { cfg.envs.insert(new.clone(), env_cfg); }
    if let Err(e) = cfg.save(&toml_path) {
        rollback(moved_env_root, moved_secrets);
        return Err(anyhow!("{e:#}"));
    }
    discover::inspect(&folder).as_ref().map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("Project not found after rename_env"))
}

// ---------------------------------------------------------------- sync

/// Two-way sync of one environment. Scaffolds init files, resolves the token
/// (silent re-login in password mode), then runs one reconciliation cycle
/// under `EmbedSyncOptions::default()` (pull and push, prompting on a gate
/// rather than bailing). Progress is streamed as `SyncPhase`.
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
    // TODO(task-11): no prompt route is installed here, so a gate this cycle
    // hits falls through to `read_line_coordinated`'s stdin branch and reads
    // this GUI process's stdin — which is EOF. That degrades to `Skip`/`N`,
    // which is safe but silent. Task 11 installs a route for `sync_env` too
    // (the same registry key a watch uses), so a one-shot sync prompts
    // exactly like a watch does; delete this comment there.
    let result: Result<u64> = block_on(async {
        rdc::cli::init::write_scaffold_files(&folder, &env, &api_base, org_id)?;
        let token = rdc::secrets::resolve_token(&folder, &env, &api_base).await?;
        rdc::cli::sync::embed::sync_logged(
            &folder,
            &env,
            &token,
            rdc::cli::sync::embed::EmbedSyncOptions::default(),
            Box::new(forwarder),
        )
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
    fn add_env_rejects_invalid_name() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        for bad in ["../x", "a/b", "..", ""] {
            let err = add_env(folder.display().to_string(), AddEnvInput {
                name: bad.into(), api_base: "https://x.test/api/v1".into(), org_id: 9,
                auth_kind: AuthKind::Token, token: Some("t".into()), username: None, password: None,
            }).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("required") || msg.contains("letters, digits"),
                "unexpected error for {bad:?}: {msg}"
            );
        }
        // Nothing outside the project folder was touched, and rdc.toml still
        // has only the original env.
        let cfg = rdc::config::ProjectConfig::load(&folder.join("rdc.toml")).unwrap();
        assert_eq!(cfg.envs.keys().collect::<Vec<_>>(), vec!["main"]);
    }

    #[test]
    fn remove_env_rejects_invalid_name() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let err = remove_env(folder.display().to_string(), "../../etc".into()).unwrap_err();
        assert!(format!("{err:#}").contains("letters, digits"));
        // The (legitimate) project is entirely untouched.
        assert!(folder.join("rdc.toml").exists());
    }

    #[test]
    fn remove_env_drops_one_and_keeps_project() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        add_env(folder.display().to_string(), AddEnvInput {
            name: "prod".into(), api_base: "https://p.test/api/v1".into(), org_id: 2,
            auth_kind: AuthKind::Token, token: Some("t2".into()), username: None, password: None,
        }).unwrap();
        // Seed on-disk artifacts the cleanup is supposed to remove, so the
        // cleanup lines are actually exercised (not just the rdc.toml edit).
        let paths = rdc::paths::Paths::for_env(&folder, "prod");
        std::fs::create_dir_all(paths.env_root()).unwrap();
        std::fs::write(paths.env_root().join("organization.json"), "{}").unwrap();
        std::fs::create_dir_all(paths.lockfile().parent().unwrap()).unwrap();
        std::fs::write(paths.lockfile(), "{}").unwrap();
        std::fs::write(paths.env_lock(), "").unwrap();
        std::fs::create_dir_all(paths.base_cache_root()).unwrap();
        std::fs::write(paths.base_cache_root().join("organization.json"), "{}").unwrap();
        let conflicts_dir = folder.join(".rdc/conflicts/prod");
        std::fs::create_dir_all(&conflicts_dir).unwrap();
        std::fs::write(conflicts_dir.join("stray.json"), "{}").unwrap();

        let p = remove_env(folder.display().to_string(), "prod".into()).unwrap().unwrap();
        assert_eq!(p.envs.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), vec!["main"]);
        assert!(!folder.join("secrets/prod.secrets.json").exists());
        assert!(!paths.env_root().exists(), "envs/prod should be removed");
        assert!(!paths.lockfile().exists(), "state lockfile.json should be removed");
        assert!(!paths.env_lock().exists(), "state .lock should be removed");
        assert!(!paths.base_cache_root().exists(), "base cache should be removed");
        assert!(!conflicts_dir.exists(), "conflicts shadow dir should be removed");
    }

    #[test]
    fn remove_last_env_trashes_project_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let r = remove_env(folder.display().to_string(), "main".into()).unwrap();
        assert!(r.is_none());
        assert!(!folder.exists()); // whole project gone (moved to trash)
    }

    #[test]
    fn add_project_uses_the_given_first_env_name() {
        let tmp = tempfile::tempdir().unwrap();
        let p = add_project(
            tmp.path().display().to_string(),
            "Acme".into(),
            AddEnvInput { name: "prod".into(), api_base: "https://p.test/api/v1/".into(), org_id: 7,
                auth_kind: AuthKind::Token, token: Some("t".into()), username: None, password: None },
        ).unwrap();
        assert_eq!(p.envs.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), vec!["prod"]);
        assert_eq!(p.envs[0].api_base, "https://p.test/api/v1"); // trailing slash trimmed
        let folder = tmp.path().join(&p.id);
        assert!(folder.join("secrets/prod.secrets.json").exists());
        assert!(!folder.join("secrets/main.secrets.json").exists());
    }

    #[test]
    fn add_project_rejects_invalid_first_env_name() {
        let tmp = tempfile::tempdir().unwrap();
        let err = add_project(tmp.path().display().to_string(), "Acme".into(),
            AddEnvInput { name: "../x".into(), api_base: "https://p.test/api/v1".into(), org_id: 1,
                auth_kind: AuthKind::Token, token: Some("t".into()), username: None, password: None }).unwrap_err();
        assert!(format!("{err:#}").to_lowercase().contains("environment name"));
    }

    #[test]
    fn rename_env_moves_files_toml_and_mapping() {
        let tmp = tempfile::tempdir().unwrap();
        // seed a 2-env project with a mapping.toml referencing `dev`
        let folder = tmp.path().join("acme");
        std::fs::create_dir_all(folder.join("envs/dev/queues")).unwrap();
        std::fs::write(folder.join("envs/dev/queues/x.json"), "{}").unwrap();
        std::fs::write(folder.join("rdc.toml"),
            "[envs.dev]\napi_base = \"https://d.test/api/v1\"\norg_id = 1\n\
             [envs.prod]\napi_base = \"https://p.test/api/v1\"\norg_id = 2\n").unwrap();
        rdc::secrets::write_secrets_file(&folder, "dev", "tok", None).unwrap();
        std::fs::create_dir_all(folder.join(".rdc")).unwrap();
        std::fs::write(folder.join(".rdc/mapping.toml"),
            "version = 2\n[[queues]]\ndev = \"cost-dev\"\nprod = \"cost-prod\"\n").unwrap();

        let p = rename_env(folder.display().to_string(), "dev".into(), "sandbox".into()).unwrap();
        let names: Vec<String> = p.envs.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"sandbox".to_string()) && names.contains(&"prod".to_string()));
        assert!(!names.contains(&"dev".to_string()));
        assert!(folder.join("envs/sandbox/queues/x.json").exists());
        assert!(!folder.join("envs/dev").exists());
        assert!(folder.join("secrets/sandbox.secrets.json").exists());
        let mapping = std::fs::read_to_string(folder.join(".rdc/mapping.toml")).unwrap();
        assert!(mapping.contains("sandbox = \"cost-dev\"") && !mapping.contains("dev = \"cost-dev\""));
    }

    #[test]
    fn rename_env_rollback_is_symmetric_on_mapping_load_failure() {
        // A corrupt `.rdc/mapping.toml` fails to parse, so `rename_env` errors
        // out at the mapping-rewrite step — AFTER the env_root/secrets moves
        // and the four best-effort cache moves (lockfile, env_lock,
        // base_cache_root, conflicts dir) have already happened. The rollback
        // must reverse ALL of them, not just env_root/secrets, or the tree is
        // left half-renamed (derived caches under `sandbox` while `rdc.toml`
        // still names `dev`).
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("acme");
        std::fs::create_dir_all(folder.join("envs/dev")).unwrap();
        std::fs::write(folder.join("envs/dev/organization.json"), "{}").unwrap();
        std::fs::write(folder.join("rdc.toml"),
            "[envs.dev]\napi_base = \"https://d.test/api/v1\"\norg_id = 1\n\
             [envs.prod]\napi_base = \"https://p.test/api/v1\"\norg_id = 2\n").unwrap();
        rdc::secrets::write_secrets_file(&folder, "dev", "tok", None).unwrap();

        let paths = rdc::paths::Paths::for_env(&folder, "dev");
        std::fs::create_dir_all(paths.lockfile().parent().unwrap()).unwrap();
        std::fs::write(paths.lockfile(), "{}").unwrap();
        std::fs::write(paths.env_lock(), "").unwrap();
        std::fs::create_dir_all(paths.base_cache_root()).unwrap();
        std::fs::write(paths.base_cache_root().join("organization.json"), "{}").unwrap();
        let conflicts_dir = folder.join(".rdc/conflicts/dev");
        std::fs::create_dir_all(&conflicts_dir).unwrap();
        std::fs::write(conflicts_dir.join("stray.json"), "{}").unwrap();

        // Garbage (unparsable) mapping.toml forces `GenericMapping::load` to
        // error inside `rename_env`, after the moves above already ran.
        std::fs::write(folder.join(".rdc/mapping.toml"), "not [ valid toml").unwrap();

        let err = rename_env(folder.display().to_string(), "dev".into(), "sandbox".into())
            .unwrap_err();
        assert!(format!("{err:#}").contains("parsing"), "unexpected error: {err:#}");

        // Everything must be back under `dev` — no `sandbox` remnants.
        assert!(folder.join("envs/dev/organization.json").exists());
        assert!(!folder.join("envs/sandbox").exists());
        assert!(folder.join("secrets/dev.secrets.json").exists());
        assert!(!folder.join("secrets/sandbox.secrets.json").exists());
        assert!(paths.lockfile().exists(), "lockfile should be rolled back to dev");
        assert!(paths.env_lock().exists(), "env_lock should be rolled back to dev");
        assert!(paths.base_cache_root().join("organization.json").exists(),
            "base cache should be rolled back to dev");
        assert!(conflicts_dir.join("stray.json").exists(), "conflicts dir should be rolled back to dev");
        let sandbox_paths = rdc::paths::Paths::for_env(&folder, "sandbox");
        assert!(!sandbox_paths.lockfile().exists());
        assert!(!sandbox_paths.env_lock().exists());
        assert!(!sandbox_paths.base_cache_root().exists());
        assert!(!folder.join(".rdc/conflicts/sandbox").exists());
        // rdc.toml untouched (rename never got past the mapping step).
        let cfg = rdc::config::ProjectConfig::load(&folder.join("rdc.toml")).unwrap();
        assert!(cfg.envs.contains_key("dev") && !cfg.envs.contains_key("sandbox"));
    }

    #[test]
    fn rename_env_rejects_collision_and_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("acme");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("rdc.toml"),
            "[envs.dev]\napi_base = \"https://d.test/api/v1\"\norg_id = 1\n\
             [envs.prod]\napi_base = \"https://p.test/api/v1\"\norg_id = 2\n").unwrap();
        assert!(rename_env(folder.display().to_string(), "dev".into(), "prod".into()).is_err()); // collision
        assert!(rename_env(folder.display().to_string(), "dev".into(), "a/b".into()).is_err());  // invalid
        assert!(rename_env(folder.display().to_string(), "nope".into(), "x".into()).is_err());   // unknown old
    }

    #[test]
    fn rename_env_rejects_invalid_old_name() {
        // `old` is never user-typed in the normal UI flow (it comes from the
        // existing project's own env list), but `validate_existing_project`
        // adopts a hand-edited `rdc.toml` with *any* env key, so a malicious
        // or corrupted `old` must be rejected before it's interpolated into
        // any filesystem path — mirrors `remove_env_rejects_invalid_name`.
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        for bad in ["../x", "a/b", ".."] {
            let err = rename_env(folder.display().to_string(), bad.into(), "sandbox".into()).unwrap_err();
            assert!(
                format!("{err:#}").contains("letters, digits"),
                "unexpected error for old={bad:?}: {err:#}"
            );
        }
        // Nothing was touched: the (legitimate) project still has only its
        // original env, untouched on disk.
        let cfg = rdc::config::ProjectConfig::load(&folder.join("rdc.toml")).unwrap();
        assert_eq!(cfg.envs.keys().collect::<Vec<_>>(), vec!["main"]);
    }
}
