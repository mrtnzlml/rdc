//! Remove the `.rdc/` artifacts of envs that are no longer in `rdc.toml`.
//!
//! See `docs/superpowers/specs/2026-11-16-rdc-dir-cleanup-design.md`.

use crate::config::ProjectConfig;
use std::path::{Path, PathBuf};

use crate::cli::sync::lock::EnvLock;
use crate::config::valid_env_name;
use std::collections::BTreeMap;

/// Suffixes under `.rdc/state/` that belong to an env, and whether each one
/// is a directory.
const STATE_SHAPES: [(&str, bool); 3] = [(".lock.json", false), (".lock", false), (".base", true)];

/// Remove (or, on `dry_run`, list) every orphaned `.rdc/` artifact: the
/// `.rdc/state/<env>.{lock.json,lock,base}` and `.rdc/conflicts/<env>/` of an
/// env that no key of `rdc.toml` names (compared case-insensitively, since on a
/// case-insensitive filesystem `Dev.lock.json` *is* env `dev`'s lockfile), and
/// the obsolete `.rdc/cache`. An env whose `<env>.lock` another process holds
/// is skipped. Entries of any other shape are left alone. Returns the paths
/// removed (or that would be), sorted; a removal failure is a warning and the
/// path is left out.
pub fn prune_orphans(root: &Path, cfg: &ProjectConfig, dry_run: bool) -> Vec<PathBuf> {
    let rdc = root.join(".rdc");
    let state = rdc.join("state");
    let conflicts = rdc.join("conflicts");
    let known = |name: &str| cfg.envs.keys().any(|k| k.eq_ignore_ascii_case(name));

    // Candidate paths grouped by orphaned env, so a held lock skips them all.
    let mut by_env: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for entry in crate::vfs::read_dir(&state).unwrap_or_default() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let Some(env) = STATE_SHAPES.iter().find_map(|(suffix, is_dir)| {
            name.strip_suffix(suffix).filter(|env| ft.is_dir() == *is_dir && valid_env_name(env))
        }) else {
            continue;
        };
        if !known(env) {
            by_env.entry(env.to_string()).or_default().push(entry.path());
        }
    }
    for entry in crate::vfs::read_dir(&conflicts).unwrap_or_default() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() && !ft.is_symlink() && valid_env_name(&name) && !known(&name) {
            by_env.entry(name).or_default().push(entry.path());
        }
    }

    let mut targets: Vec<PathBuf> = Vec::new();
    for (env, paths) in by_env {
        let lock_path = state.join(format!("{env}.lock"));
        // Only probe a lock that exists: `try_acquire` creates the file. The
        // probe is released before the removal, because Windows cannot delete
        // an open file; the env is gone from `rdc.toml`, so no new run can
        // take it in between.
        if !dry_run && crate::vfs::exists(&lock_path) {
            match EnvLock::try_acquire(&lock_path) {
                Ok(Some(_free)) => {}
                _ => continue,
            }
        }
        targets.extend(paths);
    }
    let cache = rdc.join("cache");
    if crate::vfs::symlink_metadata(&cache).is_ok() {
        targets.push(cache);
    }
    targets.sort();

    let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
    let mut done = Vec::new();
    for path in targets {
        if dry_run {
            eprintln!("would remove {}", rel(&path));
            done.push(path);
            continue;
        }
        let is_dir = crate::vfs::symlink_metadata(&path).map(|ft| ft.is_dir()).unwrap_or(false);
        let result =
            if is_dir { crate::vfs::remove_dir_all(&path) } else { crate::vfs::remove_file(&path) };
        match result {
            Ok(()) => {
                eprintln!("removed {}", rel(&path));
                done.push(path);
            }
            Err(e) => eprintln!("warning: could not remove {}: {e}", rel(&path)),
        }
    }
    if !dry_run && !done.is_empty() {
        // Succeeds only while the directory is empty.
        let _ = crate::vfs::remove_dir(&state);
        let _ = crate::vfs::remove_dir(&conflicts);
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::sync::lock::EnvLock;
    use crate::config::EnvConfig;
    use tempfile::TempDir;

    fn cfg() -> ProjectConfig {
        let mut c = ProjectConfig::default();
        c.envs.insert("dev".into(), EnvConfig { api_base: "https://example.invalid/api/v1".into(), org_id: 1 });
        c
    }

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "{}").unwrap();
    }

    fn orphan_set(root: &Path) -> Vec<PathBuf> {
        touch(root, ".rdc/state/old.lock.json");
        touch(root, ".rdc/state/old.lock");
        touch(root, ".rdc/state/old.base/queues/x.json");
        touch(root, ".rdc/conflicts/old/a.json");
        std::fs::create_dir_all(root.join(".rdc/cache")).unwrap();
        let mut v: Vec<PathBuf> = [
            ".rdc/cache",
            ".rdc/conflicts/old",
            ".rdc/state/old.base",
            ".rdc/state/old.lock",
            ".rdc/state/old.lock.json",
        ]
        .iter()
        .map(|r| root.join(r))
        .collect();
        v.sort();
        v
    }

    #[test]
    fn removes_all_shapes_of_an_unknown_env() {
        let d = TempDir::new().unwrap();
        let expected = orphan_set(d.path());
        assert_eq!(prune_orphans(d.path(), &cfg(), false), expected);
        for p in &expected {
            assert!(!p.exists(), "{} should be gone", p.display());
        }
    }

    #[test]
    fn keeps_known_envs() {
        let d = TempDir::new().unwrap();
        touch(d.path(), ".rdc/state/dev.lock.json");
        touch(d.path(), ".rdc/state/dev.base/q.json");
        touch(d.path(), ".rdc/conflicts/dev/a.json");
        assert!(prune_orphans(d.path(), &cfg(), false).is_empty());
        assert!(d.path().join(".rdc/state/dev.lock.json").exists());
        assert!(d.path().join(".rdc/state/dev.base/q.json").exists());
        assert!(d.path().join(".rdc/conflicts/dev/a.json").exists());
    }

    #[test]
    fn matches_env_names_case_insensitively() {
        let d = TempDir::new().unwrap();
        touch(d.path(), ".rdc/state/Dev.lock.json");
        assert!(prune_orphans(d.path(), &cfg(), false).is_empty());
        assert!(d.path().join(".rdc/state/Dev.lock.json").exists());
    }

    #[test]
    fn dry_run_removes_nothing() {
        let d = TempDir::new().unwrap();
        let expected = orphan_set(d.path());
        assert_eq!(prune_orphans(d.path(), &cfg(), true), expected);
        for p in &expected {
            assert!(p.exists(), "{} should survive a dry run", p.display());
        }
    }

    #[test]
    fn ignores_unknown_shapes() {
        let d = TempDir::new().unwrap();
        for rel in [
            ".rdc/state/dev.lock.json.bak",
            ".rdc/state/notes.txt",
            ".rdc/mapping.toml",
            ".rdc/map/old-to-dev.toml",
            ".rdc/state/old.base.old/x.json",
        ] {
            touch(d.path(), rel);
        }
        assert!(prune_orphans(d.path(), &cfg(), false).is_empty());
        for rel in [
            ".rdc/state/dev.lock.json.bak",
            ".rdc/state/notes.txt",
            ".rdc/mapping.toml",
            ".rdc/map/old-to-dev.toml",
            ".rdc/state/old.base.old/x.json",
        ] {
            assert!(d.path().join(rel).exists(), "{rel} should survive");
        }
    }

    #[test]
    fn does_not_create_a_lock_file() {
        let d = TempDir::new().unwrap();
        touch(d.path(), ".rdc/state/old.lock.json");
        prune_orphans(d.path(), &cfg(), false);
        assert!(!d.path().join(".rdc/state/old.lock.json").exists());
        assert!(!d.path().join(".rdc/state/old.lock").exists());
    }

    #[test]
    fn skips_an_env_whose_lock_is_held() {
        let d = TempDir::new().unwrap();
        touch(d.path(), ".rdc/state/old.lock.json");
        touch(d.path(), ".rdc/state/old.lock");
        let _held = EnvLock::try_acquire(&d.path().join(".rdc/state/old.lock")).unwrap().unwrap();
        assert!(prune_orphans(d.path(), &cfg(), false).is_empty());
        assert!(d.path().join(".rdc/state/old.lock.json").exists());
        assert!(d.path().join(".rdc/state/old.lock").exists());
    }

    #[test]
    fn removes_emptied_dirs_only() {
        let d = TempDir::new().unwrap();
        touch(d.path(), ".rdc/state/old.lock.json");
        touch(d.path(), ".rdc/conflicts/old/a.json");
        prune_orphans(d.path(), &cfg(), false);
        assert!(!d.path().join(".rdc/state").exists());
        assert!(!d.path().join(".rdc/conflicts").exists());

        let d = TempDir::new().unwrap();
        touch(d.path(), ".rdc/state/old.lock.json");
        touch(d.path(), ".rdc/state/dev.lock.json");
        prune_orphans(d.path(), &cfg(), false);
        assert!(d.path().join(".rdc/state/dev.lock.json").exists());
    }

    #[test]
    fn no_rdc_dir_is_a_no_op() {
        let d = TempDir::new().unwrap();
        assert!(prune_orphans(d.path(), &cfg(), false).is_empty());
        assert!(!d.path().join(".rdc").exists());
    }
}
