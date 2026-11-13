//! A [`StateStore`] over a host's storage, for a host whose project tree lives
//! in memory (the browser). rdc does no storage I/O here: a
//! [`SnapshotBackend`] loads and saves one opaque snapshot per env
//! (`state::snapshot`), and this store decides when and what.
//!
//! The files are the env's sync state — `.rdc/state/<env>.lock.json`,
//! `.rdc/state/<env>.base/**`, `.rdc/conflicts/<env>/**` — and its tree,
//! `envs/<env>/**`, keyed by their path relative to the project root with `/`
//! separators. One snapshot holds both, so the state and the tree the host
//! stores are always from the same checkpoint: after a closed tab the tree
//! comes back with its lockfile, instead of every object reading as a local
//! delete.
//!
//! A hydrate decides per file which side wins (`plan_hydrate`). The state
//! files are always the stored ones. The env tree is the stored one on the
//! first hydrate of a store; after that it is the tree's, except for files
//! another run changed since this store's last read or write, which are
//! merged three ways with that snapshot as the base.
//!
//! A save names the `rev` it expects and the one it writes; the backend
//! stores it only if the stored rev is still the expected one, and says so.
//! When it is not, the run fails: another run changed the state since this
//! store's hydrate.
//!
//! The snapshot comes from the host's storage, so a hydrate refuses one
//! naming a path outside this env's files before it writes anything.

use crate::paths::Paths;
use crate::rt::BoxFuture;
use crate::state::store::StateStore;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// What a [`SnapshotBackend::load`] finds: the stored `(rev, snapshot)`, or
/// `None` when nothing is stored.
pub type Stored = Option<(u64, Vec<u8>)>;

/// Where a host keeps each env's snapshot. `Send + Sync` like [`StateStore`];
/// its futures are local on wasm32, as [`crate::rt::BoxFuture`] is.
pub trait SnapshotBackend: Send + Sync {
    /// The stored `(rev, snapshot)` of `env`, or `None` when nothing is stored.
    fn load<'a>(&'a self, env: &'a str) -> BoxFuture<'a, Result<Stored>>;
    /// Store `snapshot` under `next_rev` if the stored rev is `expected_rev`
    /// (`None`: nothing stored), atomically. `Ok(false)`: it was not.
    fn save<'a>(
        &'a self,
        env: &'a str,
        snapshot: Vec<u8>,
        expected_rev: Option<u64>,
        next_rev: u64,
    ) -> BoxFuture<'a, Result<bool>>;
}

/// A stored snapshot as read or written: its `rev` and each file's SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Manifest {
    rev: u64,
    files: BTreeMap<String, String>,
}

pub struct SnapshotStateStore {
    backend: Arc<dyn SnapshotBackend>,
    /// Per env, the snapshot this store last read or wrote; `None` when it
    /// found none. An env absent here was never hydrated by this store. It
    /// is what the tree agreed with then, the base of a hydrate's merge.
    known: Mutex<BTreeMap<String, Option<Manifest>>>,
}

fn shas(files: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    files.iter().map(|(rel, text)| (rel.clone(), crate::state::raw_content_hash(text.as_bytes()))).collect()
}

impl SnapshotStateStore {
    pub fn new(backend: Arc<dyn SnapshotBackend>) -> Self {
        Self { backend, known: Mutex::default() }
    }

    fn remember(&self, env: &str, manifest: Option<Manifest>) {
        self.known.lock().unwrap_or_else(|p| p.into_inner()).insert(env.to_string(), manifest);
    }

    fn last_known(&self, env: &str) -> Option<Manifest> {
        self.known_entry(env).flatten()
    }

    fn known_entry(&self, env: &str) -> Option<Option<Manifest>> {
        self.known.lock().unwrap_or_else(|p| p.into_inner()).get(env).cloned()
    }

    async fn hydrate_env(&self, paths: &Paths) -> Result<()> {
        let env = paths.env();
        let Some((rev, bytes)) = self.backend.load(env).await? else {
            // Nothing stored yet: the local files stay, and the first save
            // publishes them.
            self.remember(env, None);
            return Ok(());
        };
        let contents = crate::state::snapshot::decode(&bytes).context("reading the stored snapshot")?;
        let m = Manifest { rev, files: shas(&contents) };
        for rel in m.files.keys() {
            if !is_persisted_path(rel, env) {
                bail!("the stored state names '{rel}', which is not a file of env '{env}'; refusing to write it");
            }
        }
        let local: BTreeMap<String, String> = persisted_files(paths)?
            .into_iter()
            .map(|(rel, bytes)| (rel, crate::state::raw_content_hash(&bytes)))
            .collect();
        let plan = plan_hydrate(env, &local, &m, self.known_entry(env).as_ref())?;
        for rel in &plan.remove {
            crate::vfs::remove_file(paths.root().join(rel)).with_context(|| format!("removing {rel}"))?;
        }
        for rel in &plan.write {
            let path = paths.root().join(rel);
            if let Some(dir) = path.parent() {
                crate::vfs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
            crate::vfs::write(&path, &contents[rel]).with_context(|| format!("writing {rel}"))?;
        }
        self.remember(env, Some(m));
        Ok(())
    }

    async fn checkpoint_env(&self, paths: &Paths) -> Result<()> {
        let env = paths.env();
        let texts = persisted_texts(paths)?;
        let files = shas(&texts);
        let known = self.last_known(env);
        match &known {
            Some(m) if m.files == files => return Ok(()),
            None if files.is_empty() => return Ok(()),
            _ => {}
        }
        let snapshot = crate::state::snapshot::encode(&texts)?;
        let expected = known.as_ref().map(|m| m.rev);
        let next = Manifest { rev: expected.map_or(1, |r| r + 1), files };
        if !self.backend.save(env, snapshot, expected, next.rev).await? {
            bail!("the state of env '{env}' was changed by another run");
        }
        self.remember(env, Some(next));
        Ok(())
    }
}

impl StateStore for SnapshotStateStore {
    fn hydrate<'a>(&'a self, paths: &'a Paths) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.hydrate_env(paths).await.with_context(|| format!("loading the sync state of env '{}'", paths.env()))
        })
    }

    fn checkpoint<'a>(&'a self, paths: &'a Paths) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.checkpoint_env(paths).await.with_context(|| format!("saving the sync state of env '{}'", paths.env()))
        })
    }
}

/// What a hydrate writes and removes, relpaths.
#[derive(Debug, Default, PartialEq, Eq)]
struct HydratePlan {
    write: BTreeSet<String>,
    remove: BTreeSet<String>,
}

/// Decide a hydrate. `local` is the tree's files (relpath → SHA-256), `m` the
/// manifest now in Data Storage, `known` what this store last read or wrote
/// (`None`: never hydrated; `Some(None)`: found no manifest then).
///
/// - The state files become exactly `m`'s.
/// - `envs/<env>/**` becomes exactly `m`'s on a first hydrate; is left alone
///   when `m` is still `known` (the tree's changes since are local ones);
///   and is merged three ways otherwise, `known` as the base: a file only
///   `m` changed takes `m`'s side, a file only the tree changed keeps its
///   own, `_index.md` (generated) takes `m`'s, and a file both changed
///   differently is an error naming it.
fn plan_hydrate(
    env: &str,
    local: &BTreeMap<String, String>,
    m: &Manifest,
    known: Option<&Option<Manifest>>,
) -> Result<HydratePlan> {
    let theirs = |rel: &str| m.files.get(rel).map(String::as_str);
    let mut plan = HydratePlan::default();
    let take = |plan: &mut HydratePlan, rel: &str| match (local.get(rel).map(String::as_str), theirs(rel)) {
        (l, t) if l == t => {}
        (_, Some(_)) => {
            plan.write.insert(rel.to_string());
        }
        (Some(_), None) => {
            plan.remove.insert(rel.to_string());
        }
        (None, None) => {}
    };
    let rels: BTreeSet<&String> = local.keys().chain(m.files.keys()).collect();
    let base: Option<&BTreeMap<String, String>> = match known {
        None => None,
        Some(k) if k.as_ref() == Some(m) => {
            for rel in rels.iter().filter(|r| is_state_file(r, env)) {
                take(&mut plan, rel);
            }
            return Ok(plan);
        }
        Some(k) => Some(k.as_ref().map(|k| &k.files).unwrap_or(&EMPTY)),
    };
    let index = format!("envs/{env}/_index.md");
    let mut both: Vec<&str> = Vec::new();
    for rel in rels {
        let Some(base) = base.filter(|_| !is_state_file(rel, env)) else {
            take(&mut plan, rel);
            continue;
        };
        let (ours, theirs_now, was) =
            (local.get(rel).map(String::as_str), theirs(rel), base.get(rel).map(String::as_str));
        if ours == theirs_now || theirs_now == was {
            continue; // agreed, or only the tree changed it
        }
        if ours == was || *rel == index {
            take(&mut plan, rel);
        } else {
            both.push(rel);
        }
    }
    if !both.is_empty() {
        bail!(
            "{} file(s) of env '{env}' were changed here and by another run since this project last \
             synced env '{env}': {}. Open the project again without envs/{env}/ to take the other \
             run's version.",
            both.len(),
            both.join(", ")
        );
    }
    Ok(plan)
}

static EMPTY: BTreeMap<String, String> = BTreeMap::new();

/// The env's files in the vfs — its sync state and `envs/<env>/**` —
/// relpath → bytes.
fn persisted_files(paths: &Paths) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut found = Vec::new();
    if crate::vfs::is_file(paths.lockfile()) {
        found.push(paths.lockfile());
    }
    walk(&paths.base_cache_root(), &mut found)?;
    walk(&paths.root().join(".rdc").join("conflicts").join(paths.env()), &mut found)?;
    walk(&paths.env_root(), &mut found)?;
    let mut out = BTreeMap::new();
    for path in found {
        let rel = relpath(paths.root(), &path)?;
        let bytes = crate::vfs::read(&path).with_context(|| format!("reading {rel}"))?;
        out.insert(rel, bytes);
    }
    Ok(out)
}

/// [`persisted_files`] as text. A file that is not UTF-8 is an error naming it.
fn persisted_texts(paths: &Paths) -> Result<BTreeMap<String, String>> {
    persisted_files(paths)?
        .into_iter()
        .map(|(rel, bytes)| match String::from_utf8(bytes) {
            Ok(text) => Ok((rel, text)),
            Err(_) => Err(anyhow!("{rel} is not UTF-8; Data Storage keeps text only")),
        })
        .collect()
}

/// Whether `rel` is a plain relative path under `prefix`.
fn plainly_under(rel: &str, prefix: &str) -> bool {
    rel.strip_prefix(prefix)
        .is_some_and(|rest| rest.split('/').all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains('\\')))
}

/// Whether `rel` is one of `env`'s state files: the lockfile, or a plain
/// relative path under its base cache or its conflict shadows.
fn is_state_file(rel: &str, env: &str) -> bool {
    rel == format!(".rdc/state/{env}.lock.json")
        || plainly_under(rel, &format!(".rdc/state/{env}.base/"))
        || plainly_under(rel, &format!(".rdc/conflicts/{env}/"))
}

/// Whether `rel` is a file this store keeps for `env`: a state file, or a
/// plain relative path under `envs/<env>/`.
fn is_persisted_path(rel: &str, env: &str) -> bool {
    is_state_file(rel, env) || plainly_under(rel, &format!("envs/{env}/"))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match crate::vfs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    for entry in entries {
        if entry.file_type()?.is_dir() {
            walk(&entry.path(), out)?;
        } else {
            out.push(entry.path());
        }
    }
    Ok(())
}

fn relpath(root: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(root).with_context(|| format!("{} is outside the project", path.display()))?;
    let parts: Vec<&str> = rel
        .components()
        .map(|c| c.as_os_str().to_str().ok_or_else(|| anyhow!("{} is not UTF-8", path.display())))
        .collect::<Result<_>>()?;
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_paths_are_this_envs_state_and_tree() {
        for ok in [
            ".rdc/state/dev.lock.json",
            ".rdc/state/dev.base/hooks/x.py",
            ".rdc/conflicts/dev/labels/a.json",
            "envs/dev/hooks/x.py",
            "envs/dev/overlay.toml",
        ] {
            assert!(is_persisted_path(ok, "dev"), "{ok}");
        }
        for bad in [
            "envs/test/hooks/x.py",
            "envs/dev/../test/hooks/x.py",
            "envs/dev",
            "../x",
            ".rdc/state/test.lock.json",
            ".rdc/state/dev.base/../../../envs/dev/hooks/x.py",
            ".rdc/conflicts/dev//x",
            ".rdc/conflicts/dev/./x",
            ".rdc/state/dev.lock",
            ".rdc/mapping.toml",
            "secrets/dev.secrets.json",
            "rdc.toml",
        ] {
            assert!(!is_persisted_path(bad, "dev"), "{bad}");
        }
        assert!(!is_state_file("envs/dev/hooks/x.py", "dev"));
    }

    fn manifest(rev: u64, files: &[(&str, &str)]) -> Manifest {
        Manifest {
            rev,
            files: files.iter().map(|(r, sha)| (r.to_string(), sha.to_string())).collect(),
        }
    }

    fn local(files: &[(&str, &str)]) -> BTreeMap<String, String> {
        files.iter().map(|(r, sha)| (r.to_string(), sha.to_string())).collect()
    }

    fn set(rels: &[&str]) -> BTreeSet<String> {
        rels.iter().map(|r| r.to_string()).collect()
    }

    const LOCK: &str = ".rdc/state/dev.lock.json";

    #[test]
    fn a_first_hydrate_takes_the_manifest_whole() {
        let m = manifest(3, &[(LOCK, "l3"), ("envs/dev/a.json", "a3"), ("envs/dev/overlay.toml", "o3")]);
        let tree = local(&[("envs/dev/overlay.toml", "scaffold"), ("envs/dev/stale.json", "s"), (".rdc/state/dev.base/x", "x")]);
        let plan = plan_hydrate("dev", &tree, &m, None).unwrap();
        assert_eq!(plan.write, set(&[LOCK, "envs/dev/a.json", "envs/dev/overlay.toml"]));
        assert_eq!(plan.remove, set(&["envs/dev/stale.json", ".rdc/state/dev.base/x"]));
    }

    #[test]
    fn an_unchanged_manifest_leaves_the_tree_and_restores_the_state() {
        let m = manifest(3, &[(LOCK, "l3"), ("envs/dev/a.json", "a3")]);
        let tree = local(&[(LOCK, "edited"), ("envs/dev/a.json", "host edit"), ("envs/dev/new.json", "n")]);
        let plan = plan_hydrate("dev", &tree, &m, Some(&Some(m.clone()))).unwrap();
        assert_eq!(plan, HydratePlan { write: set(&[LOCK]), remove: BTreeSet::new() });
        // A tree with no change at all needs nothing, not even a download.
        let same = local(&[(LOCK, "l3"), ("envs/dev/a.json", "a3")]);
        assert_eq!(plan_hydrate("dev", &same, &m, Some(&Some(m.clone()))).unwrap(), HydratePlan::default());
    }

    #[test]
    fn a_newer_manifest_merges_three_ways() {
        let k = manifest(3, &[(LOCK, "l3"), ("envs/dev/theirs.json", "t3"), ("envs/dev/ours.json", "o3"), ("envs/dev/gone.json", "g3"), ("envs/dev/_index.md", "i3")]);
        let m = manifest(5, &[(LOCK, "l5"), ("envs/dev/theirs.json", "t5"), ("envs/dev/ours.json", "o3"), ("envs/dev/added.json", "n5"), ("envs/dev/_index.md", "i5")]);
        let tree = local(&[
            (LOCK, "l3"),
            ("envs/dev/theirs.json", "t3"),
            ("envs/dev/ours.json", "o4"),
            ("envs/dev/gone.json", "g3"),
            ("envs/dev/_index.md", "i4"),
            ("envs/dev/mine.json", "m4"),
        ]);
        let plan = plan_hydrate("dev", &tree, &m, Some(&Some(k))).unwrap();
        assert_eq!(plan.write, set(&[LOCK, "envs/dev/theirs.json", "envs/dev/added.json", "envs/dev/_index.md"]));
        assert_eq!(plan.remove, set(&["envs/dev/gone.json"]));
    }

    #[test]
    fn a_file_both_sides_changed_refuses_the_hydrate() {
        let k = manifest(3, &[("envs/dev/a.json", "a3"), ("envs/dev/b.json", "b3")]);
        let m = manifest(4, &[("envs/dev/a.json", "a4"), ("envs/dev/b.json", "b3")]);
        let tree = local(&[("envs/dev/a.json", "mine"), ("envs/dev/b.json", "b3")]);
        let err = plan_hydrate("dev", &tree, &m, Some(&Some(k))).unwrap_err().to_string();
        assert!(err.contains("envs/dev/a.json") && !err.contains("b.json"), "{err}");
        // A deleted file the other run edited is a both-changed file too.
        let k = manifest(3, &[("envs/dev/a.json", "a3")]);
        let m = manifest(4, &[("envs/dev/a.json", "a4")]);
        assert!(plan_hydrate("dev", &local(&[]), &m, Some(&Some(k))).is_err());
    }

    #[test]
    fn a_manifest_first_seen_after_none_merges_on_an_empty_base() {
        let m = manifest(1, &[(LOCK, "l1"), ("envs/dev/a.json", "a1"), ("envs/dev/b.json", "b1")]);
        let tree = local(&[("envs/dev/a.json", "a1"), ("envs/dev/mine.json", "m")]);
        let plan = plan_hydrate("dev", &tree, &m, Some(&None)).unwrap();
        assert_eq!(plan.write, set(&[LOCK, "envs/dev/b.json"]));
        assert!(plan.remove.is_empty(), "{plan:?}");
    }

}
