//! Embedding entry point for non-CLI consumers (e.g. the Rossum Local
//! macOS app).
//!
//! Bypasses two CLI-only assumptions:
//! - `std::env::current_dir()` for locating `rdc.toml` and the snapshot.
//! - On-disk `secrets/<env>.secrets.json` for the API token.
//!
//! The caller supplies both explicitly. Everything else (fetch,
//! decode, atomic write, lockfile, `_index.md`) reuses the existing
//! sync pipeline in no-push, non-interactive mode.

use crate::cli::resolve::ConflictStrategy;
use crate::cli::sync::CycleOutcome;
use crate::log::Log;
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Run one no-push reconciliation cycle.
///
/// - `cwd`: project root containing `rdc.toml`.
/// - `env`: env name (the desktop app always uses `"main"`).
/// - `token`: pre-resolved API token; the secrets file is not touched.
///
/// Returns `CycleOutcome` with per-class counts. Errors propagate as
/// `anyhow::Error`; the caller surfaces them to the user.
pub async fn sync_no_push(cwd: &Path, env: &str, token: &str) -> Result<CycleOutcome> {
    let paths = crate::paths::Paths::for_env(cwd, env);
    let _lock = crate::cli::sync::lock::EnvLock::acquire(
        &paths.env_lock(),
        std::time::Duration::from_secs(30),
    )?;
    crate::cli::sync::run_cycle(
        env,
        false, // interactive
        false, // dry_run
        false, // allow_deletes
        true,  // no_push  <-- the embedding contract
        false, // no_pull
        None,  // conflict_strategy (embedding never resolves BothDiverged interactively)
        None,
        Some(cwd),
        Some(token.to_string()),
    )
    .await
}

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
#[allow(clippy::too_many_arguments)]
pub async fn watch_logged(
    cwd: &Path,
    env: &str,
    api_base: &str,
    token: String,
    poll: Option<Duration>,
    log_sink: Box<dyn std::io::Write + Send>,
    route: Arc<dyn crate::cli::stdin_coord::PromptRoute>,
    cancel: crate::cli::sync::watch::CancelToken,
) -> Result<()> {
    // Installed for this thread only, for the whole watch. A cycle never
    // leaves its thread, so this scopes exactly one route per watch.
    let _route_guard = crate::cli::stdin_coord::install_route(route);

    let renderer = Log::for_sink(crate::cli::resolve::ColorMode::Color, log_sink);

    let root = cwd.to_path_buf();
    let base = api_base.to_string();
    let refresher: crate::cli::sync::watch::TokenRefresher = Arc::new(
        move |env: String| -> futures::future::BoxFuture<'static, Result<Option<String>>> {
            let root = root.clone();
            let base = base.clone();
            Box::pin(async move {
                let t = crate::secrets::force_relogin(&root, &env, &base).await?;
                Ok(Some(t))
            })
        },
    );

    crate::cli::sync::watch::run_watch_with(
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
        },
        renderer,
        cancel,
        refresher,
        None, // post_reconcile_hook: no signal handling to install
        None, // stdin_hook: prompts arrive through `route`, not stdin
    )
    .await
}
