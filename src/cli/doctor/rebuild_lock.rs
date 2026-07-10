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

    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode());
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

    // Clear the managed snapshot + base cache so the re-pull is a TRUE clean
    // rebuild. A plain re-pull writes over existing files but leaves ORPHAN
    // directories (queues carrying pre-dedup colliding slugs from an old
    // snapshot, objects since deleted on the remote, …) — the pull then matches
    // those stale dirs instead of re-deriving the canonical unique slugs, so
    // collisions/orphans survive the "rebuild". Removing the managed tree first
    // guarantees the re-pull reconstructs it exactly as a fresh pull would.
    // Non-rdc content (the deploy `overlay/`, customer files like `tests/`) is
    // left untouched.
    clear_managed_snapshot(&paths)
        .with_context(|| format!("clearing managed snapshot for '{env}' rebuild"))?;
    log.event(
        crate::log::Action::Doctor,
        "cleared stale local snapshot for a clean rebuild",
    );

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

/// Remove every rdc-managed snapshot artifact under `envs/<env>/` plus the
/// base-cache mirror, leaving non-managed content (the deploy `overlay/`,
/// customer files like `tests/`) intact. Used by the lockfile rebuild so the
/// subsequent re-pull is a TRUE clean rebuild — no orphan directory survives to
/// reintroduce a stale slug collision.
pub(crate) fn clear_managed_snapshot(paths: &Paths) -> Result<()> {
    // Kept in sync with the pull drivers' output tree.
    const MANAGED_DIRS: &[&str] =
        &["workspaces", "hooks", "rules", "labels", "engines", "workflows", "mdh"];
    const MANAGED_FILES: &[&str] = &["organization.json", "_index.md"];
    let env_root = paths.env_root();
    for dir in MANAGED_DIRS {
        let d = env_root.join(dir);
        if d.exists() {
            std::fs::remove_dir_all(&d).with_context(|| format!("removing {}", d.display()))?;
        }
    }
    for file in MANAGED_FILES {
        let f = env_root.join(file);
        if f.exists() {
            std::fs::remove_file(&f).with_context(|| format!("removing {}", f.display()))?;
        }
    }
    let base = paths.base_cache_root();
    if base.exists() {
        std::fs::remove_dir_all(&base)
            .with_context(|| format!("removing base cache {}", base.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_managed_snapshot_removes_managed_keeps_overlay_and_customer_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let env_root = paths.env_root();
        // Managed snapshot (dirs + top-level files).
        std::fs::create_dir_all(env_root.join("workspaces/ws/queues/q")).unwrap();
        std::fs::write(env_root.join("workspaces/ws/queues/q/queue.json"), b"{}").unwrap();
        std::fs::create_dir_all(env_root.join("hooks")).unwrap();
        std::fs::write(env_root.join("hooks/h.json"), b"{}").unwrap();
        std::fs::write(env_root.join("organization.json"), b"{}").unwrap();
        std::fs::write(env_root.join("_index.md"), b"x").unwrap();
        // Non-managed: deploy overlay + customer files — MUST survive.
        std::fs::create_dir_all(env_root.join("overlay")).unwrap();
        std::fs::write(env_root.join("overlay/keep.py"), b"keep").unwrap();
        std::fs::create_dir_all(env_root.join("tests")).unwrap();
        std::fs::write(env_root.join("tests/keep_test.py"), b"keep").unwrap();
        // Base-cache mirror.
        std::fs::create_dir_all(paths.base_cache_root().join("hooks")).unwrap();
        std::fs::write(paths.base_cache_root().join("hooks/h.json"), b"{}").unwrap();

        clear_managed_snapshot(&paths).unwrap();

        assert!(!env_root.join("workspaces").exists(), "managed workspaces must be removed");
        assert!(!env_root.join("hooks").exists(), "managed hooks must be removed");
        assert!(!env_root.join("organization.json").exists(), "organization.json must be removed");
        assert!(!env_root.join("_index.md").exists(), "_index.md must be removed");
        assert!(!paths.base_cache_root().exists(), "base cache must be removed");
        assert!(env_root.join("overlay/keep.py").exists(), "deploy overlay must be preserved");
        assert!(env_root.join("tests/keep_test.py").exists(), "customer files must be preserved");
    }
}
