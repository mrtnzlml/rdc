//! `rdc repair --rebuild-lock <env>` — recover from a corrupted or stale
//! lockfile by re-pulling everything from the remote (per spec §6).
//!
//! The existing lockfile (if any) is moved to
//! `.rdc/state/<env>.lock.json.bak.<unix-ts>` for safety, then `rdc pull`
//! is invoked. With no base hash, every kind treats remote as
//! authoritative and overwrites local files. Local edits are LOST in this
//! flow — the safety net is the backup snapshot the user took before
//! invoking repair (e.g. via git).

use crate::config::ProjectConfig;
use crate::paths::Paths;
use anyhow::{anyhow, Context, Result};

pub async fn run(env: &str, rebuild_lock: bool) -> Result<()> {
    if !rebuild_lock {
        return Err(anyhow!(
            "rdc repair requires --rebuild-lock (only mode supported in v1)"
        ));
    }
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");
    let cfg = ProjectConfig::load(&cfg_path)
        .with_context(|| format!("loading project config from {}", cfg_path.display()))?;
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

    // Repair always uses the spec-§16 default concurrency. We don't
    // surface the global flag here because repair is itself a recovery
    // path — the user can re-run with --concurrency on a regular pull
    // afterward if they need to.
    let concurrency = crate::cli::resolve_concurrency(None);
    crate::cli::pull::run(env, concurrency).await?;
    println!("Lockfile rebuilt for env '{env}'.");
    Ok(())
}
