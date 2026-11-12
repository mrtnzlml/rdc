//! A [`StateStore`] that keeps an env's sync state in Rossum Data Storage, for
//! a host whose project tree lives in memory (the browser).
//!
//! One collection holds, per `(key, env)`:
//!
//! - blob documents `{_id: "<key>/<env>/blob/<sha256>/<n>", text}`: a state
//!   file's UTF-8 content in chunks of at most `chunk_bytes`, addressed by
//!   the content's SHA-256, so an upload is idempotent;
//! - one manifest `{_id: "<key>/<env>", rev, files: {"<relpath>": {sha256,
//!   chunks}}}`, the state as of the last checkpoint.
//!
//! The state files are `.rdc/state/<env>.lock.json`, `.rdc/state/<env>.base/**`
//! and `.rdc/conflicts/<env>/**`, keyed by their path relative to the project
//! root with `/` separators.
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
//! hydrate refuses a manifest naming a path outside this env's state files
//! before it writes anything. `data/bulk_write` is never used: its completion
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

/// The collection a host keeps the sync state in. Its `__` prefix is the one
/// Rossum's own collections carry, which a sync never mirrors as an MDH
/// dataset (`cli::pull::mdh::is_internal_collection`).
pub const STATE_COLLECTION: &str = "__rdc_state";

/// Blob uploads in flight at once; the client's bucket does the pacing.
const UPLOAD_FANOUT: usize = 8;
/// Blob ids per `data/find` on hydrate: about 4 MiB at 512 KiB chunks.
const FIND_BATCH: usize = 8;

/// One state file in a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileRef {
    sha256: String,
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
    /// found none. An env absent here was never hydrated by this store.
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

    fn blob_id(&self, env: &str, sha256: &str, n: usize) -> String {
        format!("{}{sha256}/{n}", self.blob_prefix(env))
    }

    fn blob_ids(&self, env: &str, files: &BTreeMap<String, FileRef>) -> BTreeSet<String> {
        files.values().flat_map(|f| (0..f.chunks).map(|n| self.blob_id(env, &f.sha256, n))).collect()
    }

    fn remember(&self, env: &str, manifest: Option<Manifest>) {
        self.known.lock().unwrap_or_else(|p| p.into_inner()).insert(env.to_string(), manifest);
    }

    fn last_known(&self, env: &str) -> Option<Manifest> {
        self.known.lock().unwrap_or_else(|p| p.into_inner()).get(env).cloned().flatten()
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

    /// The manifest and the files it names, checked to be this env's state
    /// files. `Ok(Err(_))`: a blob the manifest names is missing.
    async fn read_state(&self, paths: &Paths) -> Result<std::result::Result<(Option<Manifest>, BTreeMap<String, String>), anyhow::Error>> {
        let env = paths.env();
        let Some(m) = self.read_manifest(env).await? else {
            return Ok(Ok((None, BTreeMap::new())));
        };
        for rel in m.files.keys() {
            if !is_state_path(rel, env) {
                bail!("the manifest names '{rel}', which is not a sync state file of env '{env}'; refusing to write it");
            }
        }
        let ids: Vec<String> = self.blob_ids(env, &m.files).into_iter().collect();
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
        for (rel, f) in &m.files {
            let mut text = String::new();
            for n in 0..f.chunks {
                let id = self.blob_id(env, &f.sha256, n);
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
        Ok(Ok((Some(m), contents)))
    }

    async fn hydrate_env(&self, paths: &Paths) -> Result<()> {
        let env = paths.env();
        // A blob can vanish under a reader: a run that published since our
        // manifest read cleans up the blobs it replaced. Read once more.
        let (manifest, contents) = match self.read_state(paths).await? {
            Ok(read) => read,
            Err(_vanished) => self.read_state(paths).await??,
        };
        if manifest.is_none() {
            // Nothing published yet: local state, if any, stays, and the
            // first checkpoint publishes it.
            self.remember(env, None);
            return Ok(());
        }
        // Make the state files exactly the manifest's set.
        for rel in state_files(paths)?.keys() {
            if !contents.contains_key(rel) {
                crate::vfs::remove_file(paths.root().join(rel)).with_context(|| format!("removing {rel}"))?;
            }
        }
        for (rel, text) in &contents {
            let path = paths.root().join(rel);
            if let Some(dir) = path.parent() {
                crate::vfs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
            crate::vfs::write(&path, text).with_context(|| format!("writing {rel}"))?;
        }
        self.remember(env, manifest);
        Ok(())
    }

    async fn checkpoint_env(&self, paths: &Paths) -> Result<()> {
        let env = paths.env();
        let texts = state_files(paths)?;
        let files: BTreeMap<String, FileRef> = texts
            .iter()
            .map(|(rel, text)| {
                let chunks = chunk(text, self.chunk_bytes).len();
                (rel.clone(), FileRef { sha256: crate::state::raw_content_hash(text.as_bytes()), chunks })
            })
            .collect();
        let known = self.last_known(env);
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
            let sha = &files[rel].sha256;
            for (n, part) in chunk(text, self.chunk_bytes).into_iter().enumerate() {
                let id = self.blob_id(env, sha, n);
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
}

/// The env's state files in the vfs, relpath → content. A file that is not
/// UTF-8 is an error naming it.
fn state_files(paths: &Paths) -> Result<BTreeMap<String, String>> {
    let mut found = Vec::new();
    if crate::vfs::is_file(paths.lockfile()) {
        found.push(paths.lockfile());
    }
    walk(&paths.base_cache_root(), &mut found)?;
    walk(&paths.root().join(".rdc").join("conflicts").join(paths.env()), &mut found)?;
    let mut out = BTreeMap::new();
    for path in found {
        let rel = relpath(paths.root(), &path)?;
        let bytes = crate::vfs::read(&path).with_context(|| format!("reading {rel}"))?;
        let text = String::from_utf8(bytes).map_err(|_| anyhow!("{rel} is not UTF-8; Data Storage keeps text only"))?;
        out.insert(rel, text);
    }
    Ok(out)
}

/// Whether `rel` is one of `env`'s state files: the lockfile, or a plain
/// relative path under its base cache or its conflict shadows.
fn is_state_path(rel: &str, env: &str) -> bool {
    if rel == format!(".rdc/state/{env}.lock.json") {
        return true;
    }
    let under = [format!(".rdc/state/{env}.base/"), format!(".rdc/conflicts/{env}/")];
    under.iter().any(|p| {
        rel.strip_prefix(p.as_str())
            .is_some_and(|rest| rest.split('/').all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains('\\')))
    })
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
    fn state_paths_are_this_envs_state_files() {
        for ok in [".rdc/state/dev.lock.json", ".rdc/state/dev.base/hooks/x.py", ".rdc/conflicts/dev/labels/a.json"] {
            assert!(is_state_path(ok, "dev"), "{ok}");
        }
        for bad in [
            "envs/dev/hooks/x.py",
            "../x",
            ".rdc/state/test.lock.json",
            ".rdc/state/dev.base/../../../envs/dev/hooks/x.py",
            ".rdc/conflicts/dev//x",
            ".rdc/conflicts/dev/./x",
            ".rdc/state/dev.lock",
            "rdc.toml",
        ] {
            assert!(!is_state_path(bad, "dev"), "{bad}");
        }
    }

}
