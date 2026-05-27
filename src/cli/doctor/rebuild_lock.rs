use crate::config::ProjectConfig;
use crate::paths::Paths;
use anyhow::{anyhow, Context, Result};

/// Online recovery: back up the existing lockfile and re-pull from
/// remote. Local snapshot files are overwritten with remote
/// contents; local edits not present on remote are LOST. The safety
/// net is whatever backup the user took before invoking doctor
/// (e.g. via git).
pub async fn run(env: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)?;
    if !cfg.envs.contains_key(env) {
        return Err(anyhow!("env '{env}' is not defined in rdc.toml"));
    }

    let paths = Paths::for_env(&cwd, env);
    let lockfile_path = paths.lockfile();

    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode(false));
    if lockfile_path.exists() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut backup = lockfile_path.clone();
        let new_name = format!(
            "{}.bak.{now}",
            backup.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("lock.json"),
        );
        backup.set_file_name(new_name);
        std::fs::rename(&lockfile_path, &backup)
            .with_context(|| format!("backing up lockfile to {}", backup.display()))?;
        log.event(crate::log::Action::Doctor, &format!("backed up lockfile to {}", backup.display()));
        log.event(crate::log::Action::Info, "rdc sync will now overwrite local snapshot files with remote contents");
    } else {
        log.event(crate::log::Action::Info, &format!("no existing lockfile at {}; proceeding with fresh sync", lockfile_path.display()));
    }

    // The rebuild is non-interactive: with no merge base every kind's
    // three-way collapses to "Write", so there's nothing to resolve.
    // Sync with `--no-push` is the post-unified-sync equivalent of the
    // old `pull` flow — every remote item lands as `RemoteCreate` and
    // the pull-side dispatcher writes it.
    crate::cli::sync::run(
        env, /* interactive */ false, /* dry_run */ false,
        /* allow_deletes */ false, /* no_push */ true, /* no_pull */ false,
    )
    .await?;
    log.event(crate::log::Action::Doctor, &format!("done env '{env}' rebuilt"));
    Ok(())
}
