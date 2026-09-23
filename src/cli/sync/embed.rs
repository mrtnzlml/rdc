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
