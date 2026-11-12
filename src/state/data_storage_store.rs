//! A [`StateStore`] that keeps an env's sync state in Rossum Data Storage, for
//! a host whose project tree lives in memory (the browser).
//!
//! One collection holds, per `(key, env)`:
//!
//! - blob documents `{_id: "<key>/<env>/blob/<sha256>/<chunk_bytes>/<n>",
//!   text}`: a file's UTF-8 content in chunks of at most `chunk_bytes`,
//!   addressed by the content's SHA-256 and the chunk size, so an upload is
//!   idempotent and two stores with different chunk sizes never share an id
//!   for different bytes;
//! - one manifest `{_id: "<key>/<env>", rev, files: {"<relpath>": {sha256,
//!   chunk_bytes, chunks}}}`, the state as of the last checkpoint. A file
//!   unchanged since the last manifest keeps that manifest's chunking.
//!
//! The files are the env's sync state — `.rdc/state/<env>.lock.json`,
//! `.rdc/state/<env>.base/**`, `.rdc/conflicts/<env>/**` — and its tree,
//! `envs/<env>/**`, keyed by their path relative to the project root with `/`
//! separators. One manifest holds both, so the state and the tree in Data
//! Storage are always from the same checkpoint: after a closed tab the tree
//! comes back with its lockfile, instead of every object reading as a local
//! delete.
//!
//! A hydrate decides per file which side wins (`plan_hydrate`). The state
//! files are always the manifest's. The env tree is the manifest's on the
//! first hydrate of a store; after that it is the tree's, except for files
//! another run changed since this store's last read or write, which are
//! merged three ways with that manifest as the base.
//!
//! A checkpoint uploads the blobs first and publishes them with one write of
//! the manifest, a compare-and-swap on its `rev`: a run that dies between the
//! two leaves the previous manifest, whose blobs are untouched, and a run
//! whose state another run changed since its hydrate is refused.
//!
//! After a publish the store deletes the blobs of the manifest it replaced
//! that the new one does not reference: no run can publish from a replaced
//! manifest any more (the compare-and-swap refuses it). It never deletes a
//! blob it did not see in a manifest, so blobs a dead run uploaded and never
//! published stay behind; they are unreferenced and harmless. A failed
//! clean-up is a warning, not a failed run ([`StateStore::take_warnings`]).
//!
//! The manifest is data anyone with Data Storage access can write, so a
//! hydrate refuses a manifest naming a path outside this env's files before
//! it writes anything. `data/bulk_write` is never used: its completion
//! cannot be observed.

use crate::api::data_storage::DataStorageClient;
use crate::paths::Paths;
use crate::rt::BoxFuture;
use crate::state::store::StateStore;
use anyhow::{anyhow, bail, Context, Result};
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The default collection a host keeps the sync state in. Its `__` prefix is
/// the one Rossum's own collections carry; a sync never mirrors it, nor any
/// other collection a store is configured with, as an MDH dataset
/// (`cli::pull::mdh::is_internal_collection`).
pub const STATE_COLLECTION: &str = "__rdc_state";

/// Blob uploads in flight at once; the client's bucket does the pacing.
const UPLOAD_FANOUT: usize = 8;
/// Blob ids per `data/find` on hydrate: about 4 MiB at 512 KiB chunks.
const FIND_BATCH: usize = 8;

/// One state file in a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileRef {
    sha256: String,
    /// The chunk size the file was split with; part of its blob ids.
    chunk_bytes: usize,
    chunks: usize,
}

/// A manifest as read or written: its `rev` and its files.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Manifest {
    rev: u64,
    files: BTreeMap<String, FileRef>,
}

pub struct DataStorageStateStore {
    /// Behind a lock so [`StateStore::use_token`] can swap its token; each
    /// call works on a clone.
    client: Mutex<DataStorageClient>,
    collection: String,
    key: String,
    chunk_bytes: usize,
    /// Per env, the manifest this store last read or wrote; `None` when it
    /// found none. An env absent here was never hydrated by this store. It
    /// is what the tree agreed with then, the base of a hydrate's merge.
    known: Mutex<BTreeMap<String, Option<Manifest>>>,
    /// Warnings not taken yet ([`StateStore::take_warnings`]).
    warnings: Mutex<Vec<String>>,
}

impl DataStorageStateStore {
    /// `key` names the project (the host knows its org and project);
    /// `chunk_bytes` bounds a blob document's `text`.
    pub fn new(client: DataStorageClient, collection: String, key: String, chunk_bytes: usize) -> Self {
        Self { client: Mutex::new(client), collection, key, chunk_bytes: chunk_bytes.max(1), known: Mutex::default(), warnings: Mutex::default() }
    }

    fn client(&self) -> DataStorageClient {
        self.client.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn manifest_id(&self, env: &str) -> String {
        format!("{}/{env}", self.key)
    }

    fn blob_prefix(&self, env: &str) -> String {
        format!("{}/{env}/blob/", self.key)
    }

    fn blob_id(&self, env: &str, f: &FileRef, n: usize) -> String {
        format!("{}{}/{}/{n}", self.blob_prefix(env), f.sha256, f.chunk_bytes)
    }

    fn blob_ids(&self, env: &str, files: &BTreeMap<String, FileRef>) -> BTreeSet<String> {
        files.values().flat_map(|f| (0..f.chunks).map(|n| self.blob_id(env, f, n))).collect()
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

    async fn read_manifest(&self, env: &str) -> Result<Option<Manifest>> {
        let docs = self
            .client()
            .find(&self.collection, &json!({ "_id": self.manifest_id(env) }), None, None)
            .await?;
        let Some(doc) = docs.into_iter().next() else {
            return Ok(None);
        };
        let rev = doc.get("rev").and_then(Value::as_u64).ok_or_else(|| anyhow!("the manifest has no `rev`"))?;
        let files = serde_json::from_value(doc.get("files").cloned().unwrap_or(Value::Null))
            .context("decoding the manifest's `files`")?;
        Ok(Some(Manifest { rev, files }))
    }

    /// The texts of `files` from their blobs, checked against their hashes.
    /// `Ok(Err(_))`: a blob is missing.
    async fn read_blobs(
        &self,
        env: &str,
        files: &BTreeMap<String, FileRef>,
    ) -> Result<std::result::Result<BTreeMap<String, String>, anyhow::Error>> {
        let ids: Vec<String> = self.blob_ids(env, files).into_iter().collect();
        let mut texts: BTreeMap<String, String> = BTreeMap::new();
        for batch in ids.chunks(FIND_BATCH) {
            let docs = self
                .client()
                .find(&self.collection, &json!({ "_id": { "$in": batch } }), None, None)
                .await?;
            for d in docs {
                if let (Some(id), Some(text)) = (d.get("_id").and_then(Value::as_str), d.get("text").and_then(Value::as_str)) {
                    texts.insert(id.to_string(), text.to_string());
                }
            }
        }
        let mut contents = BTreeMap::new();
        for (rel, f) in files {
            let mut text = String::new();
            for n in 0..f.chunks {
                let id = self.blob_id(env, f, n);
                match texts.get(&id) {
                    Some(t) => text.push_str(t),
                    None => return Ok(Err(anyhow!("{rel}: blob {id} is missing"))),
                }
            }
            if crate::state::raw_content_hash(text.as_bytes()) != f.sha256 {
                bail!("{rel}: its blobs do not hash to the manifest's sha256");
            }
            contents.insert(rel.clone(), text);
        }
        Ok(Ok(contents))
    }

    async fn hydrate_env(&self, paths: &Paths) -> Result<()> {
        let env = paths.env();
        // A blob can vanish under a reader: a run that published since our
        // manifest read cleans up the blobs it replaced. Read once more.
        let mut retried = false;
        loop {
            let Some(m) = self.read_manifest(env).await? else {
                // Nothing published yet: the local files stay, and the first
                // checkpoint publishes them.
                self.remember(env, None);
                return Ok(());
            };
            for rel in m.files.keys() {
                if !is_persisted_path(rel, env) {
                    bail!("the manifest names '{rel}', which is not a file of env '{env}'; refusing to write it");
                }
            }
            let local: BTreeMap<String, String> = persisted_files(paths)?
                .into_iter()
                .map(|(rel, bytes)| (rel, crate::state::raw_content_hash(&bytes)))
                .collect();
            let plan = plan_hydrate(env, &local, &m, self.known_entry(env).as_ref())?;
            let wanted: BTreeMap<String, FileRef> =
                plan.write.iter().map(|rel| (rel.clone(), m.files[rel].clone())).collect();
            let contents = match self.read_blobs(env, &wanted).await? {
                Ok(c) => c,
                Err(e) if retried => return Err(e),
                Err(_vanished) => {
                    retried = true;
                    continue;
                }
            };
            for rel in &plan.remove {
                crate::vfs::remove_file(paths.root().join(rel)).with_context(|| format!("removing {rel}"))?;
            }
            for (rel, text) in &contents {
                let path = paths.root().join(rel);
                if let Some(dir) = path.parent() {
                    crate::vfs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
                }
                crate::vfs::write(&path, text).with_context(|| format!("writing {rel}"))?;
            }
            self.remember(env, Some(m));
            return Ok(());
        }
    }

    async fn checkpoint_env(&self, paths: &Paths) -> Result<()> {
        let env = paths.env();
        let texts = persisted_texts(paths)?;
        let known = self.last_known(env);
        // A file the last manifest already holds with these bytes keeps its
        // chunking there (and so its blobs); anything else is split with this
        // store's chunk size.
        let files: BTreeMap<String, FileRef> = texts
            .iter()
            .map(|(rel, text)| {
                let sha256 = crate::state::raw_content_hash(text.as_bytes());
                let kept = known.as_ref().and_then(|m| m.files.get(rel)).filter(|f| f.sha256 == sha256).cloned();
                let f = kept.unwrap_or_else(|| FileRef {
                    chunks: chunk(text, self.chunk_bytes).len(),
                    chunk_bytes: self.chunk_bytes,
                    sha256,
                });
                (rel.clone(), f)
            })
            .collect();
        match &known {
            Some(m) if m.files == files => return Ok(()),
            None if files.is_empty() => return Ok(()),
            _ => {}
        }

        // Blobs first, each an idempotent upsert by `_id`; the ones the last
        // manifest references are there already.
        let have = known.as_ref().map(|m| self.blob_ids(env, &m.files)).unwrap_or_default();
        let mut uploads: BTreeMap<String, String> = BTreeMap::new();
        for (rel, text) in &texts {
            let f = &files[rel];
            for (n, part) in chunk(text, f.chunk_bytes).into_iter().enumerate() {
                let id = self.blob_id(env, f, n);
                if !have.contains(&id) {
                    uploads.insert(id, part.to_string());
                }
            }
        }
        futures::stream::iter(uploads)
            .map(|(id, text)| async move {
                self.client()
                    .replace_where(&self.collection, &json!({ "_id": id }), &json!({ "text": text }), true, None)
                    .await
                    .with_context(|| format!("uploading blob {id}"))
                    .map(drop)
            })
            .buffer_unordered(UPLOAD_FANOUT)
            .try_collect::<Vec<()>>()
            .await?;

        // Then the manifest, compare-and-swap on the rev this store last saw.
        let next = Manifest { rev: known.as_ref().map_or(1, |m| m.rev + 1), files };
        let id = self.manifest_id(env);
        let doc = json!({ "rev": next.rev, "files": next.files });
        let written = match &known {
            Some(m) => self
                .client()
                .replace_where(&self.collection, &json!({ "_id": id, "rev": m.rev }), &doc, false, None)
                .await
                .map(|out| out.matched_count == 1),
            None => {
                let mut first = doc.clone();
                first["_id"] = json!(id);
                self.client().insert_many(&self.collection, &[first], None).await.map(|()| true)
            }
        };
        // Not written (no match, or an error such as a duplicate `_id` on
        // the first manifest): the manifest now says what happened. Ours →
        // a write whose answer was lost; the one we saw → the error stands;
        // any other → another run's.
        let failed = match written {
            Ok(true) => None,
            Ok(false) => Some(None),
            Err(e) => Some(Some(e)),
        };
        if let Some(error) = failed {
            let now = match self.read_manifest(env).await {
                Ok(now) => now,
                Err(e) => {
                    let first = error.map_or_else(|| "the compare-and-swap matched nothing".to_string(), |e| format!("{e:#}"));
                    return Err(e.context(format!("re-reading the manifest after its write failed ({first})")));
                }
            };
            if now.as_ref() != Some(&next) {
                match error {
                    Some(e) if now == known => return Err(e),
                    _ => bail!("the state of env '{env}' was changed by another run"),
                }
            }
        }
        let referenced = self.blob_ids(env, &next.files);
        self.remember(env, Some(next));

        // Clean up the replaced manifest's blobs the new one does not use.
        // Never fails the run: the state is published either way.
        let stale: Vec<Value> = have.difference(&referenced).map(|id| json!(id)).collect();
        if !stale.is_empty()
            && let Err(e) = self.client().delete_many_by_ids(&self.collection, &stale, None).await
        {
            self.warnings.lock().unwrap_or_else(|p| p.into_inner()).push(format!(
                "the sync state of env '{env}' was saved, but deleting {} replaced blob(s) from Data Storage failed: {e:#}",
                stale.len()
            ));
        }
        Ok(())
    }
}

impl StateStore for DataStorageStateStore {
    fn hydrate<'a>(&'a self, paths: &'a Paths) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.hydrate_env(paths)
                .await
                .with_context(|| format!("loading the sync state of env '{}' from Data Storage", paths.env()))
        })
    }

    fn checkpoint<'a>(&'a self, paths: &'a Paths) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.checkpoint_env(paths)
                .await
                .with_context(|| format!("saving the sync state of env '{}' to Data Storage", paths.env()))
        })
    }

    fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.lock().unwrap_or_else(|p| p.into_inner()))
    }

    fn use_token(&self, token: &str) {
        self.client.lock().unwrap_or_else(|p| p.into_inner()).set_token(token.to_string());
    }

    fn collection(&self) -> Option<String> {
        Some(self.collection.clone())
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
    let theirs = |rel: &str| m.files.get(rel).map(|f| f.sha256.as_str());
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
    let base: Option<&BTreeMap<String, FileRef>> = match known {
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
            (local.get(rel).map(String::as_str), theirs(rel), base.get(rel).map(|f| f.sha256.as_str()));
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

static EMPTY: BTreeMap<String, FileRef> = BTreeMap::new();

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

/// `text` in pieces of at most `max` bytes, split on char boundaries (a
/// char wider than `max` is a piece of its own). Empty text is no piece.
fn chunk(text: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = max.min(rest.len());
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            end = rest.chars().next().map_or(rest.len(), char::len_utf8);
        }
        let (head, tail) = rest.split_at(end);
        out.push(head);
        rest = tail;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_split_on_char_boundaries() {
        assert_eq!(chunk("", 4), Vec::<&str>::new());
        assert_eq!(chunk("abcdef", 4), vec!["abcd", "ef"]);
        assert_eq!(chunk("a✓b", 2), vec!["a", "✓", "b"]);
        assert_eq!(chunk("✓✓", 4), vec!["✓", "✓"]);
        let text = "é✓x".repeat(9);
        assert_eq!(chunk(&text, 5).concat(), text);
        assert!(chunk(&text, 5).iter().all(|c| c.len() <= 5));
    }

    #[test]
    fn the_state_collection_is_never_a_dataset() {
        assert!(crate::cli::pull::mdh::is_internal_collection(STATE_COLLECTION));
    }

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
            files: files.iter().map(|(r, sha)| (r.to_string(), FileRef { sha256: sha.to_string(), chunk_bytes: 1024, chunks: 1 })).collect(),
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
