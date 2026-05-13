//! `rdc repair <env>` — bring the local snapshot back into a clean state.
//!
//! Two modes, one mandatory:
//!
//! * `--rebuild-lock` (online): back up the existing lockfile to
//!   `.rdc/state/<env>.lock.json.bak.<unix-ts>` and re-pull everything
//!   from the remote. With no base hash, every kind treats remote as
//!   authoritative and overwrites local files. Local edits are LOST —
//!   the safety net is the backup snapshot the user took before
//!   invoking repair (e.g. via git).
//!
//! * `--rename-slugs` (offline): rename any local file whose slug no
//!   longer matches its JSON `name` field. Pull never moves files;
//!   this is the explicit user-driven action that brings stale slugs
//!   into alignment. Cascade-aware. No API calls.

use crate::config::ProjectConfig;
use crate::paths::Paths;
use anyhow::{anyhow, Context, Result};

pub async fn run(
    env: &str,
    rebuild_lock: bool,
    rename_slugs: bool,
    check: bool,
    yes: bool,
) -> Result<()> {
    // Pick exactly one mode. No implicit default because both modes
    // touch on-disk files in irreversible ways.
    match (rebuild_lock, rename_slugs) {
        (false, false) => Err(anyhow!(
            "rdc repair needs a mode flag: --rebuild-lock or --rename-slugs"
        )),
        (true, true) => Err(anyhow!(
            "rdc repair --rebuild-lock and --rename-slugs are mutually exclusive"
        )),
        (true, false) => {
            if check {
                return Err(anyhow!(
                    "rdc repair --rebuild-lock does not support --check (it always re-pulls). \
                     Use git to preview what a rebuild would overwrite."
                ));
            }
            rebuild_lock_run(env).await
        }
        (false, true) => rename_slugs_run(env, check, yes).await,
    }
}

async fn rebuild_lock_run(env: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)?;
    if !cfg.envs.contains_key(env) {
        return Err(anyhow!("env '{env}' is not defined in rdc.toml"));
    }

    let paths = Paths::for_env(&cwd, env);
    let lockfile_path = paths.lockfile();

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
        eprintln!("Backed up existing lockfile to {}", backup.display());
        eprintln!("Note: rdc pull will now overwrite local snapshot files with remote contents.");
    } else {
        eprintln!("No existing lockfile at {} — proceeding with fresh pull.", lockfile_path.display());
    }

    // Repair is non-interactive: with no merge base every kind's
    // three-way collapses to "Write", so there's nothing to resolve.
    crate::cli::pull::run(env, false).await?;
    println!("Lockfile rebuilt for env '{env}'.");
    Ok(())
}

async fn rename_slugs_run(env: &str, check: bool, yes: bool) -> Result<()> {
    // Delegates to the existing within-env realign flow. Stays offline:
    // no pull, no API call. The realign module knows the cascade rules
    // (workspace rename moves the whole queues subtree, etc.).
    crate::cli::deploy::realign::run_within_env(env, check, yes).await
}
