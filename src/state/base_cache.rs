//! Sidecar cache of last-synced bytes — the "base" leg of sync's
//! 3-way merge.
//!
//! Layout (mirrors the env tree one-to-one):
//!
//! ```text
//! <root>/.rdc/state/<env>.base/
//! ├── organization.json
//! ├── hooks/validator.json
//! ├── hooks/validator.py            ← code sidecar
//! ├── rules/vat-check.json
//! ├── rules/vat-check.py
//! ├── labels/priority-high.json
//! ├── engines/invoices/engine.json
//! ├── engines/invoices/fields/amount.json
//! ├── workflows/ap-flow/workflow.json
//! ├── workflows/ap-flow/steps/approval.json
//! └── workspaces/<ws>/queues/<q>/{queue,schema,inbox}.json
//!                                /schema.formulas/<datapoint>.py
//!                                /email-templates/<tpl>.json
//! ```
//!
//! Writers (pull / push / deploy) call [`write`] every time they
//! commit an authoritative snapshot to disk — alongside the env-tree
//! file, never instead of it. Readers (sync's conflict resolver)
//! call [`read`] to look up the base for a 3-way merge. Doctor's
//! cache-GC sub-step calls [`prune_orphans`] to remove cache files
//! whose owning slug/kind no longer appears in the lockfile.
//!
//! Hash invariant: for any file in the cache,
//! `content_hash(canonicalize(cache_bytes)) ==
//! lockfile.objects[kind][slug].content_hash` for the entry that
//! recorded it. Drift between the two is a bug; sync re-records
//! after every authoritative write.

use crate::paths::Paths;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Translate an env-tree path (`envs/<env>/hooks/x.json`) to its cache
/// mirror (`.rdc/state/<env>.base/hooks/x.json`). Returns `None` if
/// `env_file` isn't inside `paths.env_root()` (caller bug).
pub fn cache_mirror(paths: &Paths, env_file: &Path) -> Option<PathBuf> {
    let rel = env_file.strip_prefix(paths.env_root()).ok()?;
    Some(paths.base_cache_root().join(rel))
}

/// Commit `bytes` as the new base for `env_file`. Writes the cache
/// mirror atomically; the parent dir tree is created as needed.
///
/// Cheap to call even when nothing changed — `write_atomic` skips the
/// rename when bytes already match, so re-pulling a clean env doesn't
/// bump mtimes.
pub fn write(paths: &Paths, env_file: &Path, bytes: &[u8]) -> Result<()> {
    let Some(mirror) = cache_mirror(paths, env_file) else {
        // Caller passed a path outside the env tree. Silently
        // ignoring would mask a misuse; return an error so the
        // mistake surfaces in tests.
        anyhow::bail!(
            "base_cache::write: {} is not under env_root {}",
            env_file.display(),
            paths.env_root().display()
        );
    };
    crate::snapshot::writer::write_atomic(&mirror, bytes)
        .with_context(|| format!("writing base cache {}", mirror.display()))
}

/// Load the cached base bytes for `env_file`. `Ok(None)` when the
/// cache mirror doesn't exist — the caller (sync's conflict
/// resolver) treats that as "no base available, can't 3-way-merge,
/// fall through to interactive prompt".
pub fn read(paths: &Paths, env_file: &Path) -> Result<Option<Vec<u8>>> {
    let Some(mirror) = cache_mirror(paths, env_file) else {
        return Ok(None);
    };
    if !mirror.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&mirror)
        .with_context(|| format!("reading base cache {}", mirror.display()))?;
    Ok(Some(bytes))
}

/// Remove the cache mirror for `env_file` if present. Used when an
/// object is deleted (push tombstone, remote-delete adoption); the
/// orphan cache file would otherwise survive until the next GC pass.
pub fn forget(paths: &Paths, env_file: &Path) -> Result<()> {
    let Some(mirror) = cache_mirror(paths, env_file) else { return Ok(()) };
    if mirror.exists() {
        std::fs::remove_file(&mirror)
            .with_context(|| format!("removing base cache {}", mirror.display()))?;
    }
    Ok(())
}

/// Walk the cache root and delete any file whose corresponding
/// env-tree path doesn't exist (i.e. the source file was removed
/// from the working snapshot). Returns the count removed.
///
/// Conservative: only deletes files whose env-tree counterpart is
/// missing. Files that exist in the cache but not in the lockfile
/// (perhaps a hand-edit, or a transient state) are left alone — the
/// lockfile isn't authoritative for the cache, the on-disk env is.
pub fn prune_orphans(paths: &Paths) -> Result<usize> {
    let cache_root = paths.base_cache_root();
    if !cache_root.exists() {
        return Ok(0);
    }
    let env_root = paths.env_root();
    let mut removed = 0;
    walk_files(&cache_root, &mut |cache_file| -> Result<()> {
        let rel = match cache_file.strip_prefix(&cache_root) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        let env_counterpart = env_root.join(rel);
        if !env_counterpart.exists() {
            std::fs::remove_file(cache_file)
                .with_context(|| format!("pruning base cache {}", cache_file.display()))?;
            removed += 1;
        }
        Ok(())
    })?;
    // Sweep empty directories left behind by the file removals.
    prune_empty_dirs(&cache_root)?;
    Ok(removed)
}

fn walk_files(
    root: &Path,
    f: &mut dyn FnMut(&Path) -> Result<()>,
) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
        {
            let entry = entry?;
            let ft = entry.file_type()?;
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                f(&path)?;
            }
        }
    }
    Ok(())
}

fn prune_empty_dirs(root: &Path) -> Result<()> {
    // Post-order traversal: collect dirs, sort longest first, attempt
    // remove_dir on each. `remove_dir` only succeeds for empty dirs,
    // so non-empty ones are silently skipped.
    let mut dirs = Vec::new();
    walk_dirs(root, &mut dirs)?;
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for d in dirs {
        if d == *root { continue; } // never delete the cache root itself
        let _ = std::fs::remove_dir(&d);
    }
    Ok(())
}

fn walk_dirs(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        out.push(dir.clone());
        for entry in std::fs::read_dir(&dir).context("read_dir")? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn paths_for(dir: &Path) -> Paths {
        Paths::for_env(dir, "dev")
    }

    #[test]
    fn write_then_read_round_trip() {
        let dir = TempDir::new().unwrap();
        let p = paths_for(dir.path());
        let env_file = p.env_root().join("hooks").join("x.json");
        std::fs::create_dir_all(env_file.parent().unwrap()).unwrap();
        std::fs::write(&env_file, b"{}\n").unwrap();

        write(&p, &env_file, b"{}\n").unwrap();
        let got = read(&p, &env_file).unwrap();
        assert_eq!(got.as_deref(), Some(b"{}\n" as &[u8]));
    }

    #[test]
    fn read_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        let p = paths_for(dir.path());
        let env_file = p.env_root().join("hooks").join("missing.json");
        assert!(read(&p, &env_file).unwrap().is_none());
    }

    #[test]
    fn write_outside_env_root_errors() {
        let dir = TempDir::new().unwrap();
        let p = paths_for(dir.path());
        let outside = dir.path().join("not-env").join("x.json");
        let err = write(&p, &outside, b"{}\n").unwrap_err();
        assert!(err.to_string().contains("not under env_root"));
    }

    #[test]
    fn cache_mirror_maps_env_path_to_cache_path() {
        let dir = TempDir::new().unwrap();
        let p = paths_for(dir.path());
        let env_file = p.env_root().join("workspaces").join("ws-1").join("queues").join("q-1").join("schema.json");
        let mirror = cache_mirror(&p, &env_file).unwrap();
        assert_eq!(
            mirror,
            p.base_cache_root().join("workspaces").join("ws-1").join("queues").join("q-1").join("schema.json")
        );
    }

    #[test]
    fn forget_removes_existing_cache_entry() {
        let dir = TempDir::new().unwrap();
        let p = paths_for(dir.path());
        let env_file = p.env_root().join("hooks").join("x.json");
        std::fs::create_dir_all(env_file.parent().unwrap()).unwrap();
        std::fs::write(&env_file, b"data").unwrap();
        write(&p, &env_file, b"data").unwrap();

        forget(&p, &env_file).unwrap();
        assert!(read(&p, &env_file).unwrap().is_none());
    }

    #[test]
    fn prune_orphans_removes_cache_entries_without_env_counterpart() {
        let dir = TempDir::new().unwrap();
        let p = paths_for(dir.path());

        // Existing env file + matching cache.
        let kept = p.env_root().join("hooks").join("kept.json");
        std::fs::create_dir_all(kept.parent().unwrap()).unwrap();
        std::fs::write(&kept, b"k").unwrap();
        write(&p, &kept, b"k").unwrap();

        // Cache entry with no env counterpart (file was removed).
        let orphan_env = p.env_root().join("hooks").join("orphan.json");
        write(&p, &orphan_env, b"o").unwrap();
        // Note: we DON'T create the env-tree counterpart.

        let removed = prune_orphans(&p).unwrap();
        assert_eq!(removed, 1, "should remove only the orphan");
        assert!(
            read(&p, &kept).unwrap().is_some(),
            "kept entry must survive"
        );
        assert!(
            read(&p, &orphan_env).unwrap().is_none(),
            "orphan must be gone"
        );
    }

    #[test]
    fn prune_orphans_is_noop_when_cache_root_missing() {
        let dir = TempDir::new().unwrap();
        let p = paths_for(dir.path());
        let removed = prune_orphans(&p).unwrap();
        assert_eq!(removed, 0);
    }
}
