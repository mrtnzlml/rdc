//! Embedding entry point for non-CLI consumers (e.g. the Rossum Local
//! desktop app).
//!
//! Bypasses two CLI-only assumptions:
//! - `std::env::current_dir()` for locating `rdc.toml` and the snapshot.
//! - On-disk `secrets/<env>.secrets.json` for the API token.
//!
//! The caller supplies both explicitly. Everything else (fetch, decode,
//! atomic write, lockfile, `_index.md`) reuses the existing sync/watch
//! pipeline — [`sync_logged`] runs it once under caller-supplied
//! [`EmbedSyncOptions`] (two-way by default), and [`watch_logged`] runs it
//! in a loop.

use crate::cli::resolve::ConflictStrategy;
use crate::cli::sync::CycleOutcome;
use crate::vfs::Store;
use crate::log::Log;
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Public embedding surface for interactive prompts. An embedder builds a
/// [`PromptRoute`] and passes it to [`watch_logged`], which scopes it with
/// [`with_route`] for the lifetime of the watch. The rest of
/// `cli::stdin_coord` — `StdinCoordinator`, `activate`, `arm_bell`,
/// `CoordinatorStdin`, `read_line_coordinated` — is the CLI's own
/// terminal-stdin machinery and is deliberately NOT part of this surface;
/// only these five names are re-exported.
pub use crate::cli::change_view::menu_one_line;
pub use crate::cli::sync::watch::CycleTrigger;
pub use crate::cli::stdin_coord::{Prompt, PromptKey, PromptKind, PromptRoute, with_route};

/// How an embedder wants one reconciliation cycle run.
///
/// `Default` is the desktop app's policy, and the three interlocking parts
/// of it are load-bearing: `interactive: true` so a gate prompts instead of
/// bailing, `allow_deletes: false` so the gate is actually reached, and
/// `conflict: None` so a divergence is asked about rather than decided.
/// Changing any one of them silently removes a user's say over a delete.
pub struct EmbedSyncOptions {
    pub interactive: bool,
    pub allow_deletes: bool,
    pub no_push: bool,
    pub no_pull: bool,
    pub dry_run: bool,
    pub conflict: Option<ConflictStrategy>,
}

impl Default for EmbedSyncOptions {
    fn default() -> Self {
        Self {
            interactive: true,
            allow_deletes: false,
            no_push: false,
            no_pull: false,
            dry_run: false,
            conflict: None,
        }
    }
}

/// Run one reconciliation cycle, streaming rdc's rendered log into
/// `log_sink` — one `write` per line.
///
/// Lines carry rdc's normal ANSI colour so the embedder can preserve it,
/// and the log is non-TTY (no cursor redraws). Because a renderer is
/// supplied, `run_cycle` omits its cycle-closing summary; the caller adds
/// its own completion line.
pub async fn sync_logged(
    cwd: &Path,
    env: &str,
    token: &str,
    opts: EmbedSyncOptions,
    log_sink: Box<dyn std::io::Write + Send>,
) -> Result<CycleOutcome> {
    let paths = crate::paths::Paths::for_env(cwd, env);
    let _lock = crate::cli::sync::lock::EnvLock::acquire(
        &paths.env_lock(),
        std::time::Duration::from_secs(30),
    )?;
    let renderer = Log::for_sink(crate::cli::resolve::ColorMode::Color, log_sink);
    crate::cli::sync::run_cycle(
        env,
        opts.interactive,
        opts.dry_run,
        opts.allow_deletes,
        opts.no_push,
        opts.no_pull,
        opts.conflict,
        Some(renderer),
        Some(cwd),
        Some(token.to_string()),
        false,
    )
    .await
}

/// [`sync_logged`] on an in-memory (or any other) project: every file access
/// inside goes through `store`, never the real disk.
pub async fn sync_logged_in(
    store: Arc<dyn Store>,
    root: &Path,
    env: &str,
    token: &str,
    opts: EmbedSyncOptions,
    log_sink: Box<dyn std::io::Write + Send>,
) -> Result<CycleOutcome> {
    crate::vfs::scope(store, sync_logged(root, env, token, opts, log_sink)).await
}

/// [`watch_logged`] on a project held by `store`. `triggers` is the host's
/// stand-in for file events: each message runs a cycle as a local change
/// would. A non-native store has no OS watcher, so without `triggers` only
/// polling (and the initial reconcile) runs.
pub async fn watch_logged_in(
    store: Arc<dyn Store>,
    root: &Path,
    env: &str,
    api_base: &str,
    token: String,
    poll: Option<Duration>,
    log_sink: Box<dyn std::io::Write + Send>,
    route: Arc<dyn PromptRoute>,
    cancel: crate::cli::sync::watch::CancelToken,
    on_idle: Option<Arc<dyn Fn(Option<u64>) + Send + Sync>>,
    triggers: Option<tokio::sync::mpsc::Receiver<CycleTrigger>>,
) -> Result<()> {
    crate::vfs::scope(
        store,
        watch_inner(root, env, api_base, token, poll, log_sink, route, cancel, on_idle, triggers),
    )
    .await
}

/// Run a watch loop against `cwd`/`env` until `cancel` fires, streaming the
/// log into `log_sink` and routing every blocking prompt to `route`.
///
/// Unlike `cli::sync::watch::run_watch` this owns no stdin, installs no
/// signal handler, and returns normally instead of exiting the process.
/// 401s are resolved with `secrets::force_relogin` rather than the CLI's
/// interactive refresh, because the app's credentials live in the secrets
/// file, not in `RDC_USER_<ENV>` / `RDC_PASS_<ENV>`.
///
/// `on_idle`, when given, is called once per completed cycle with the
/// configured poll interval in seconds (`None` when polling is disabled) —
/// see `WatchConfig::on_idle`. The CLI's own `run_watch` has no equivalent
/// (its in-place countdown line is TTY-only and needs no callback); an
/// embedder with no such line of its own passes one to learn when a watch
/// goes idle and render its own countdown.
pub async fn watch_logged(
    cwd: &Path,
    env: &str,
    api_base: &str,
    token: String,
    poll: Option<Duration>,
    log_sink: Box<dyn std::io::Write + Send>,
    route: Arc<dyn PromptRoute>,
    cancel: crate::cli::sync::watch::CancelToken,
    on_idle: Option<Arc<dyn Fn(Option<u64>) + Send + Sync>>,
) -> Result<()> {
    watch_inner(cwd, env, api_base, token, poll, log_sink, route, cancel, on_idle, None).await
}

async fn watch_inner(
    cwd: &Path,
    env: &str,
    api_base: &str,
    token: String,
    poll: Option<Duration>,
    log_sink: Box<dyn std::io::Write + Send>,
    route: Arc<dyn PromptRoute>,
    cancel: crate::cli::sync::watch::CancelToken,
    on_idle: Option<Arc<dyn Fn(Option<u64>) + Send + Sync>>,
    triggers: Option<tokio::sync::mpsc::Receiver<CycleTrigger>>,
) -> Result<()> {
    let renderer = Log::for_sink(crate::cli::resolve::ColorMode::Color, log_sink);

    let root = cwd.to_path_buf();
    let base = api_base.to_string();
    let refresher: crate::cli::sync::watch::TokenRefresher = Arc::new(
        move |env: String| -> crate::rt::BoxFuture<'static, Result<Option<String>>> {
            let root = root.clone();
            let base = base.clone();
            Box::pin(async move {
                let t = crate::secrets::force_relogin(&root, &env, &base).await?;
                Ok(Some(t))
            })
        },
    );

    // Scoped to exactly this call, across every `.await` the watch loop
    // makes for its whole lifetime — see `with_route`'s doc for why a
    // task-local scope (not a thread-local guard) is what makes that safe
    // under a multi-thread runtime.
    with_route(
        route,
        crate::cli::sync::watch::run_watch_with(
            watch_logged_config(env, cwd, token, poll, on_idle),
            renderer,
            cancel,
            refresher,
            None, // post_reconcile_hook: no signal handling to install
            None, // stdin_hook: prompts arrive through `route`, not stdin
            triggers,
        ),
    )
    .await
}

/// Build the `WatchConfig` [`watch_logged`] runs with. Factored out of the
/// call above so the desktop's policy is unit-testable rather than buried
/// in a struct literal nobody outside this module can observe.
///
/// This mirrors `EmbedSyncOptions::default()`'s policy above, for the same
/// reason: `interactive: true` so a pending delete or conflict prompts
/// (through `route`) instead of `bail!`-ing and killing an unattended
/// watch; `allow_deletes: false` so that gate is actually reached rather
/// than skipped; two-way (`no_push` / `no_pull` both `false`) because the
/// app's watch, like its sync, pulls and pushes. Flipping any one of these
/// removes a user's say over a delete with nothing red in CI to catch it —
/// see `watch_logged_config_prompts_rather_than_bails_and_is_two_way`.
pub(crate) fn watch_logged_config<'a>(
    env: &'a str,
    cwd: &'a Path,
    token: String,
    poll: Option<Duration>,
    on_idle: Option<Arc<dyn Fn(Option<u64>) + Send + Sync>>,
) -> crate::cli::sync::watch::WatchConfig<'a> {
    crate::cli::sync::watch::WatchConfig {
        env,
        cwd: Some(cwd),
        token: Some(token),
        interactive: true,
        allow_deletes: false,
        no_push: false,
        no_pull: false,
        poll,
        verbose: false,
        no_bell: true, // a terminal BEL means nothing in a GUI process
        on_idle,
    }
}

// ------------------------------------------------------------ project and env management
//
// The project and env bookkeeping every embedder offers: create a project,
// validate one, edit, add, remove and rename an env. Each takes the project
// `root` and does all file access through `vfs`, so the desktop bridge runs
// it on disk and the browser on an in-memory tree. Folder naming, trashing
// and the bridge DTOs stay with the embedder.

/// How an env authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    Token,
    Password,
}

/// A new env, with its credentials.
#[derive(Debug, Clone)]
pub struct EnvInput {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// New settings for an existing env. Credentials are optional: leave all
/// blank to keep the saved ones.
#[derive(Debug, Clone)]
pub struct EditEnvInput {
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// One env as an embedder lists it.
#[derive(Debug, Clone)]
pub struct EnvInfo {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    /// The lockfile's mtime. Always `None` off disk: a non-native store
    /// keeps no timestamps.
    pub last_sync_unix: Option<i64>,
    pub file_count: u64,
}

/// A project as an embedder lists it.
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    /// Sorted by name; empty only when `error` is set.
    pub envs: Vec<EnvInfo>,
    /// Why `rdc.toml` failed to load. The project is still returned so the
    /// user sees what to fix.
    pub error: Option<String>,
}

/// What [`remove_env`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovedEnv {
    /// The env and its files are gone; the project remains.
    Removed,
    /// It was the project's last env. Nothing was changed: removing it means
    /// removing the project, which is the embedder's call.
    LastEnv,
}

/// `root` as a project, or `None` when it is not one (no directory, no
/// `rdc.toml`, or no envs in it).
pub fn inspect_project(root: &Path) -> Option<ProjectInfo> {
    if !crate::vfs::is_dir(root) {
        return None;
    }
    let toml_path = root.join("rdc.toml");
    if !crate::vfs::is_file(&toml_path) {
        return None;
    }
    // rdc's own loader, so the listing applies the same env-name check as
    // every other command and never reads a path a bad key would select.
    let cfg = match crate::config::ProjectConfig::load(&toml_path) {
        Ok(cfg) => cfg,
        Err(e) => return Some(ProjectInfo { envs: Vec::new(), error: Some(format!("{e:#}")) }),
    };
    if cfg.envs.is_empty() {
        return None;
    }
    // BTreeMap iterates in sorted key order → envs come out sorted by name.
    let envs = cfg
        .envs
        .into_iter()
        .map(|(name, cfg)| {
            let paths = crate::paths::Paths::for_env(root, name.as_str());
            let secrets = crate::secrets::read_secrets_file(root, &name).unwrap_or_default();
            let auth_kind =
                if secrets.username.is_some() { AuthKind::Password } else { AuthKind::Token };
            EnvInfo {
                last_sync_unix: crate::vfs::modified_unix(paths.lockfile()),
                file_count: count_files(&paths.env_root()),
                name,
                api_base: cfg.api_base,
                org_id: cfg.org_id,
                auth_kind,
            }
        })
        .collect();
    Some(ProjectInfo { envs, error: None })
}

/// Regular files below `dir`, recursively. Symlinks are not followed or
/// counted. 0 when `dir` does not exist.
pub fn count_files(dir: &Path) -> u64 {
    fn walk(p: &Path, acc: &mut u64) {
        if let Ok(entries) = crate::vfs::read_dir(p) {
            for entry in entries {
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    walk(&entry.path(), acc);
                } else if ft.is_file() {
                    *acc += 1;
                }
            }
        }
    }
    let mut n = 0;
    walk(dir, &mut n);
    n
}

/// Check that `root` is an rdc project (at least one env, any names).
/// Moves and writes nothing.
pub fn validate_project(root: &Path) -> Result<ProjectInfo> {
    let path = root.display();
    if !crate::vfs::is_dir(root) {
        anyhow::bail!("Not a folder: {path}");
    }
    if !crate::vfs::exists(root.join("rdc.toml")) {
        anyhow::bail!(
            "{path} doesn't look like an rdc project (no rdc.toml). Run `rdc init` there first."
        );
    }
    let project = inspect_project(root)
        .ok_or_else(|| anyhow::anyhow!("{path} has no environments defined in rdc.toml."))?;
    if let Some(e) = &project.error {
        anyhow::bail!("{e}");
    }
    Ok(project)
}

/// Create a project at `root` with one env: its credentials, then
/// `rdc.toml`. The scaffold (`.gitlab-ci.yml`, …) is
/// `cli::init::write_scaffold_files`'s job, not this one's.
pub fn add_project(root: &Path, first_env: &EnvInput) -> Result<()> {
    let env_name = first_env.name.trim().to_string();
    if !crate::config::valid_env_name(&env_name) {
        anyhow::bail!(
            "Environment name may only contain letters, digits, - and _ (and can't be empty)."
        );
    }
    crate::vfs::create_dir_all(root).map_err(|e| anyhow::anyhow!("creating folder: {e}"))?;
    // Credentials first (validates + writes secrets) so a bad credential
    // never leaves a config behind.
    write_credentials(root, &env_name, first_env.auth_kind, first_env.token.as_deref(),
        first_env.username.as_deref(), first_env.password.as_deref())?;
    let mut cfg = crate::config::ProjectConfig::default();
    cfg.envs.insert(env_name, crate::config::EnvConfig {
        api_base: first_env.api_base.trim_end_matches('/').to_string(),
        org_id: first_env.org_id,
    });
    cfg.save(&root.join("rdc.toml")).map_err(|e| anyhow::anyhow!("{e:#}"))
}

/// Rewrite `env`'s api_base/org_id through rdc's own config writer, and
/// replace its credentials only when new ones are supplied.
pub fn edit_env(root: &Path, env: &str, input: &EditEnvInput) -> Result<()> {
    let toml_path = root.join("rdc.toml");
    let mut cfg =
        crate::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow::anyhow!("{e:#}"))?;
    let ec = cfg
        .envs
        .get_mut(env)
        .ok_or_else(|| anyhow::anyhow!("This project has no `{env}` environment."))?;
    ec.api_base = input.api_base.trim_end_matches('/').to_string();
    ec.org_id = input.org_id;
    cfg.save(&toml_path).map_err(|e| anyhow::anyhow!("{e:#}"))?;

    let has_new_credentials = match input.auth_kind {
        AuthKind::Token => input.token.as_deref().is_some_and(|s| !s.is_empty()),
        AuthKind::Password => {
            input.username.as_deref().is_some_and(|s| !s.is_empty())
                || input.password.as_deref().is_some_and(|s| !s.is_empty())
        }
    };
    if has_new_credentials {
        let _ = crate::vfs::remove_file(crate::paths::Paths::for_env(root, env).secrets_file());
        write_credentials(root, env, input.auth_kind, input.token.as_deref(),
            input.username.as_deref(), input.password.as_deref())?;
    }
    Ok(())
}

/// Add an env to an existing project. Errors if it already exists.
pub fn add_env(root: &Path, input: &EnvInput) -> Result<()> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        anyhow::bail!("Environment name is required.");
    }
    if !crate::config::valid_env_name(&name) {
        anyhow::bail!(crate::config::INVALID_ENV_NAME_MSG);
    }
    let toml_path = root.join("rdc.toml");
    let mut cfg =
        crate::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow::anyhow!("{e:#}"))?;
    if cfg.envs.contains_key(&name) {
        anyhow::bail!("An environment named \"{name}\" already exists in this project.");
    }
    // Validate + write credentials FIRST: if they're invalid, `rdc.toml` must
    // stay untouched so the caller can fix the input and retry without first
    // having to remove a half-registered env.
    write_credentials(root, &name, input.auth_kind, input.token.as_deref(),
        input.username.as_deref(), input.password.as_deref())?;
    cfg.envs.insert(name, crate::config::EnvConfig {
        api_base: input.api_base.trim_end_matches('/').to_string(),
        org_id: input.org_id,
    });
    cfg.save(&toml_path).map_err(|e| anyhow::anyhow!("{e:#}"))
}

/// Remove an env: its `rdc.toml` section, snapshot, secrets and state. The
/// last env is left alone and reported as [`RemovedEnv::LastEnv`].
pub fn remove_env(root: &Path, env: &str) -> Result<RemovedEnv> {
    // Guard against a malformed `rdc.toml` env key (or any other caller
    // mistake) before `env` is ever interpolated into a path below — an env
    // name with `..` or a path separator must never reach a removal.
    if !crate::config::valid_env_name(env) {
        anyhow::bail!(crate::config::INVALID_ENV_NAME_MSG);
    }
    let toml_path = root.join("rdc.toml");
    let mut cfg =
        crate::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow::anyhow!("{e:#}"))?;
    if !cfg.envs.contains_key(env) {
        anyhow::bail!("This project has no \"{env}\" environment.");
    }
    if cfg.envs.len() == 1 {
        return Ok(RemovedEnv::LastEnv);
    }
    cfg.envs.remove(env);
    cfg.save(&toml_path).map_err(|e| anyhow::anyhow!("{e:#}"))?;
    // Best-effort cleanup, all through `Paths` so the layout stays
    // single-sourced (src/paths.rs).
    let paths = crate::paths::Paths::for_env(root, env);
    let _ = crate::vfs::remove_dir_all(paths.env_root());
    let _ = crate::vfs::remove_file(paths.secrets_file());
    let _ = crate::vfs::remove_file(crate::secrets::hook_secrets_path(root, env));
    let _ = crate::vfs::remove_file(paths.lockfile());
    let _ = crate::vfs::remove_file(paths.env_lock());
    let _ = crate::vfs::remove_dir_all(paths.base_cache_root());
    // `Paths` has no bare "conflicts dir" accessor, only the per-file
    // `conflict_shadow_path`; passing the env root itself as `local_path`
    // makes it strip to an empty relpath, yielding the env's whole
    // `.rdc/conflicts/<env>/` shadow directory.
    let _ = crate::vfs::remove_dir_all(paths.conflict_shadow_path(&paths.env_root()));
    Ok(RemovedEnv::Removed)
}

/// Rename an env `old` → `new`, offline. The work is `rdc edit env rename`'s,
/// so every surface renames the same paths, refuses the same cases and rolls
/// back the same way. A rename to the same name (an edit dialog submits the
/// name even when unchanged) is a no-op with an empty report.
pub fn rename_env(root: &Path, old: &str, new: &str) -> Result<crate::cli::edit::env::RenameReport> {
    if crate::config::valid_env_name(old) && new.trim() == old {
        return Ok(crate::cli::edit::env::RenameReport::default());
    }
    crate::cli::edit::env::rename_env(root, old, new, false).map_err(|e| anyhow::anyhow!("{e:#}"))
}

/// Write credentials via rdc's own helpers. Empty strings are rejected.
fn write_credentials(
    root: &Path,
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
                .ok_or_else(|| anyhow::anyhow!("Token is required."))?;
            crate::secrets::write_secrets_file(root, env, t, None)
                .map_err(|e| anyhow::anyhow!("{e:#}"))?;
        }
        AuthKind::Password => {
            let u = username
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("Username is required."))?;
            let p = password
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("Password is required."))?;
            crate::secrets::save_password_credentials(root, env, u, p)
                .map_err(|e| anyhow::anyhow!("{e:#}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_logged_config_prompts_rather_than_bails_and_is_two_way() {
        let cwd = Path::new("/tmp/does-not-matter");
        let cfg = watch_logged_config("test", cwd, "tok".to_string(), None, None);
        assert!(
            cfg.interactive,
            "false would bail! on a pending delete or conflict and kill an unattended watch"
        );
        assert!(
            !cfg.allow_deletes,
            "true would skip the delete gate entirely"
        );
        assert!(!cfg.no_push && !cfg.no_pull, "the app's watch is two-way");
        assert!(cfg.no_bell, "a terminal BEL means nothing in a GUI process");
    }
}
