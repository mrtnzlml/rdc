//! Push driver for MDH (Master Data Hub) index edits.
//!
//! The push side of the snapshot model: when a user has edited
//! `envs/<env>/mdh/<slug>/indexes.json`, this driver computes the
//! diff against the remote's current index set and applies it via
//! `create_index` / `drop_index` (regular) and
//! `create_search_index` / `drop_search_index` (Atlas Search).
//!
//! Modify semantics: the Data Storage API has no in-place "update
//! index" verb, so a definition change is a **drop + re-create**. The
//! window between drop and re-create is brief for regular indexes;
//! Atlas Search rebuilds in the background after `create_search_index`
//! returns, so a freshly re-created search index may temporarily miss
//! results until the rebuild completes.
//!
//! The implicit `_id_` regular index is filtered from both sides of
//! the diff so users hand-editing it back into `indexes.json` doesn't
//! produce a false drop/create — the server refuses to drop `_id_`
//! anyway. Server-set `v` (index-version) field is stripped before
//! comparing definitions.

use crate::api::DataStorageClient;
use crate::log::{Action, Log};
use crate::model::IndexSet;
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Poll cadence for index-drop wait loops.
const DROP_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Maximum time to wait for a regular index drop to complete. Regular
/// (b-tree / hashed) drops on MongoDB are nearly instant; the wrapper
/// API returns 202 but the underlying op is fast. 10s is generous.
const REGULAR_DROP_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum time to wait for an Atlas Search index drop to complete.
/// Search-index teardown runs in Atlas's background and can take
/// several seconds for non-trivial mappings; 60s leaves headroom.
const SEARCH_DROP_TIMEOUT: Duration = Duration::from_secs(60);

/// Push local index edits for one MDH dataset to the remote. Drops
/// first (avoiding name collisions when a definition has changed),
/// then creates. On any API failure the function returns the error and
/// leaves the lockfile untouched, so the next sync re-classifies the
/// dataset as "local diverged from base" and the user can retry.
///
/// Returns the number of API write operations performed (drops +
/// creates) for the caller's summary line.
pub async fn push_dataset(
    client: &DataStorageClient,
    lockfile: &mut Lockfile,
    collection_name: &str,
    slug: &str,
    indexes_path: &Path,
    progress: &Arc<Log>,
) -> Result<usize> {
    let local_raw = std::fs::read(indexes_path)
        .with_context(|| format!("reading {}", indexes_path.display()))?;
    let local_set: IndexSet = serde_json::from_slice(&local_raw)
        .with_context(|| format!("parsing {}", indexes_path.display()))?;

    // Fetch the live remote state directly — don't trust the lockfile
    // baseline here, since an admin may have added indexes via the UI
    // since the last sync and we don't want to silently drop those.
    let remote_regular = client
        .list_indexes(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("listing regular indexes for '{collection_name}'"))?;
    let remote_search = client
        .list_search_indexes(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("listing search indexes for '{collection_name}'"))?;

    let plan = diff_for_dataset(
        &local_set.regular,
        &local_set.search,
        &remote_regular,
        &remote_search,
    );

    let ops = apply_diff(client, collection_name, slug, &plan, progress).await?;

    // Push fully applied — refresh the lockfile content_hash to the
    // canonical hash of the local bytes. The next pull-driver pass
    // will see remote (after our writes) canonicalize-equal to local
    // and run NoChange, so no spurious overwrite.
    if ops > 0 {
        let hash = content_hash(&local_raw);
        let map = lockfile
            .objects
            .entry("mdh_indexes".to_string())
            .or_default();
        map.insert(
            slug.to_string(),
            ObjectEntry {
                id: 0,
                url: None,
                modified_at: None,
                content_hash: Some(hash),
                secrets_hash: None,
            },
        );
    }

    Ok(ops)
}

/// Apply a computed [`DiffPlan`] to `collection_name` on `client`: drops
/// first (so a changed definition frees its name), then creates — waiting
/// for any same-name drop to finish before re-creating, since Data Storage
/// drops are async. Returns the number of API write ops performed. Shared by
/// within-env push (`push_dataset`) and cross-env deploy.
pub(crate) async fn apply_diff(
    client: &DataStorageClient,
    collection_name: &str,
    slug: &str,
    plan: &DiffPlan,
    progress: &Arc<Log>,
) -> Result<usize> {
    let mut ops = 0usize;
    // Track which names we just dropped so creates that reuse the same
    // name know to wait for the async drop to complete before issuing
    // the create. Without this gate the drop-then-create-same-name
    // sequence races: the drop is queued, the create either fails on
    // "already exists" or — worse for Atlas Search — succeeds and is
    // then clobbered when the queued drop finally fires. Cross-name
    // drop+create pairs don't race (different namespaces).
    let mut dropped_regular: BTreeSet<String> = BTreeSet::new();
    let mut dropped_search: BTreeSet<String> = BTreeSet::new();
    for name in &plan.drop_regular {
        client
            .drop_index(collection_name, name, Some(progress.clone()))
            .await
            .with_context(|| format!("dropping regular index '{name}' on '{collection_name}'"))?;
        progress.event(Action::Delete, &format!("mdh/{slug} regular index '{name}'"));
        dropped_regular.insert(name.clone());
        ops += 1;
    }
    for name in &plan.drop_search {
        client
            .drop_search_index(collection_name, name, Some(progress.clone()))
            .await
            .with_context(|| format!("dropping search index '{name}' on '{collection_name}'"))?;
        progress.event(Action::Delete, &format!("mdh/{slug} search index '{name}'"));
        dropped_search.insert(name.clone());
        ops += 1;
    }
    for def in &plan.create_regular {
        let name = def
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("regular index def missing `name` field: {def}"))?;
        if dropped_regular.contains(name) {
            wait_for_regular_drop(client, collection_name, name, progress).await?;
        }
        let keys = def
            .get("key")
            .ok_or_else(|| anyhow!("regular index '{name}' missing `key` field"))?;
        let options = def_options_only(def);
        client
            .create_index(collection_name, name, keys, &options, Some(progress.clone()))
            .await
            .with_context(|| format!("creating regular index '{name}' on '{collection_name}'"))?;
        progress.event(Action::Post, &format!("mdh/{slug} regular index '{name}'"));
        ops += 1;
    }
    for def in &plan.create_search {
        let name = def
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("search index def missing `name` field: {def}"))?;
        if dropped_search.contains(name) {
            wait_for_search_drop(client, collection_name, name, progress).await?;
        }
        let mappings = def
            .get("mappings")
            .ok_or_else(|| anyhow!("search index '{name}' missing `mappings` field"))?;
        let analyzers = def
            .get("analyzers")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));
        client
            .create_search_index(collection_name, name, mappings, &analyzers, Some(progress.clone()))
            .await
            .with_context(|| format!("creating search index '{name}' on '{collection_name}'"))?;
        progress.event(Action::Post, &format!("mdh/{slug} search index '{name}'"));
        ops += 1;
    }
    Ok(ops)
}

/// Poll `list_indexes` until the named regular index is gone (or the
/// timeout expires). Used after `drop_index` when the next step is a
/// `create_index` for the same name — without this gate the drop is
/// still pending when create runs, and the API rejects "already
/// exists" or silently clobbers the just-created definition.
async fn wait_for_regular_drop(
    client: &DataStorageClient,
    collection: &str,
    index_name: &str,
    progress: &Arc<Log>,
) -> Result<()> {
    let start = Instant::now();
    loop {
        let list = client
            .list_indexes(collection, Some(progress.clone()))
            .await
            .with_context(|| format!("polling list_indexes for '{collection}'"))?;
        let still_there = list
            .iter()
            .any(|ix| ix.get("name").and_then(|n| n.as_str()) == Some(index_name));
        if !still_there {
            return Ok(());
        }
        if start.elapsed() >= REGULAR_DROP_TIMEOUT {
            return Err(anyhow!(
                "timed out after {:?} waiting for regular index '{}' on '{}' to drop",
                REGULAR_DROP_TIMEOUT,
                index_name,
                collection
            ));
        }
        tokio::time::sleep(DROP_POLL_INTERVAL).await;
    }
}

/// Poll `list_search_indexes` until the named search index is gone
/// (or the longer Atlas-Search timeout expires). Atlas tears down the
/// underlying index asynchronously in the background, so a drop +
/// recreate of the same name MUST wait or the queued drop will
/// clobber the just-created index.
async fn wait_for_search_drop(
    client: &DataStorageClient,
    collection: &str,
    index_name: &str,
    progress: &Arc<Log>,
) -> Result<()> {
    let start = Instant::now();
    loop {
        let list = client
            .list_search_indexes(collection, Some(progress.clone()))
            .await
            .with_context(|| format!("polling list_search_indexes for '{collection}'"))?;
        let still_there = list
            .iter()
            .any(|ix| ix.get("name").and_then(|n| n.as_str()) == Some(index_name));
        if !still_there {
            return Ok(());
        }
        if start.elapsed() >= SEARCH_DROP_TIMEOUT {
            return Err(anyhow!(
                "timed out after {:?} waiting for search index '{}' on '{}' to drop",
                SEARCH_DROP_TIMEOUT,
                index_name,
                collection
            ));
        }
        tokio::time::sleep(DROP_POLL_INTERVAL).await;
    }
}

/// Pure index-set diff: which named entries should be dropped from
/// the server, and which should be created. Modifications (same name,
/// different definition) appear as both drop AND create. The implicit
/// `_id_` regular index is filtered from both sides — server-managed,
/// can't be dropped.
#[derive(Debug, Default)]
pub(crate) struct DiffPlan {
    pub drop_regular: Vec<String>,
    pub drop_search: Vec<String>,
    pub create_regular: Vec<Value>,
    pub create_search: Vec<Value>,
}

/// Always-mirror diff for the within-env push driver (make the remote index
/// set exactly match the local edit). Thin wrapper over [`diff_indexes`].
fn diff_for_dataset(
    local_regular: &[Value],
    local_search: &[Value],
    remote_regular: &[Value],
    remote_search: &[Value],
) -> DiffPlan {
    diff_indexes(local_regular, local_search, remote_regular, remote_search, true)
}

/// Pure index-set diff. `mirror` controls only the pruning of entries that
/// exist remotely but not locally:
///   - `mirror == true`  → drop remote-only entries (make remote == local
///     exactly; within-env `rdc sync` push, and `rdc deploy --mirror`).
///   - `mirror == false` → leave remote-only entries in place (additive;
///     `rdc deploy` without `--mirror`).
/// A *changed* entry (same name, diverging definition) is ALWAYS a drop +
/// create regardless of `mirror`, because the Data Storage API has no
/// in-place update verb.
pub(crate) fn diff_indexes(
    local_regular: &[Value],
    local_search: &[Value],
    remote_regular: &[Value],
    remote_search: &[Value],
    mirror: bool,
) -> DiffPlan {
    fn index_by_name(items: &[Value], filter_id_index: bool) -> BTreeMap<String, &Value> {
        let mut out: BTreeMap<String, &Value> = BTreeMap::new();
        for ix in items {
            if let Some(name) = ix.get("name").and_then(|v| v.as_str()) {
                if filter_id_index && name == "_id_" {
                    continue;
                }
                out.insert(name.to_string(), ix);
            }
        }
        out
    }
    let local_reg = index_by_name(local_regular, true);
    let remote_reg = index_by_name(remote_regular, true);
    let local_search_map = index_by_name(local_search, false);
    let remote_search_map = index_by_name(remote_search, false);

    let mut plan = DiffPlan::default();
    // Drop: present remotely but absent locally, OR present on both
    // sides with diverging definitions (will be re-created in the
    // creates pass).
    for (name, remote_def) in &remote_reg {
        match local_reg.get(name) {
            // Remote-only: prune only when mirroring.
            None => {
                if mirror {
                    plan.drop_regular.push(name.clone());
                }
            }
            // Same name, changed definition: always drop+recreate.
            Some(local_def) => {
                if !defs_equivalent(local_def, remote_def) {
                    plan.drop_regular.push(name.clone());
                }
            }
        }
    }
    for (name, local_def) in &local_reg {
        match remote_reg.get(name) {
            None => plan.create_regular.push((*local_def).clone()),
            Some(remote_def) => {
                if !defs_equivalent(local_def, remote_def) {
                    plan.create_regular.push((*local_def).clone());
                }
            }
        }
    }
    for (name, remote_def) in &remote_search_map {
        match local_search_map.get(name) {
            // Remote-only: prune only when mirroring.
            None => {
                if mirror {
                    plan.drop_search.push(name.clone());
                }
            }
            // Same name, changed definition: always drop+recreate.
            Some(local_def) => {
                if !defs_equivalent(local_def, remote_def) {
                    plan.drop_search.push(name.clone());
                }
            }
        }
    }
    for (name, local_def) in &local_search_map {
        match remote_search_map.get(name) {
            None => plan.create_search.push((*local_def).clone()),
            Some(remote_def) => {
                if !defs_equivalent(local_def, remote_def) {
                    plan.create_search.push((*local_def).clone());
                }
            }
        }
    }
    plan
}

/// Two index definitions are equivalent under the server-set `v`
/// stripping (already done at pull time on the local side, but
/// remote still has it). Key order inside nested objects is
/// canonicalized so the comparison is structural.
fn defs_equivalent(a: &Value, b: &Value) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    if let Value::Object(obj) = &mut a {
        obj.remove("v");
    }
    if let Value::Object(obj) = &mut b {
        obj.remove("v");
    }
    let canon_a = crate::snapshot::noise::canonicalize_for_hash(
        &serde_json::to_vec(&a).unwrap_or_default(),
    );
    let canon_b = crate::snapshot::noise::canonicalize_for_hash(
        &serde_json::to_vec(&b).unwrap_or_default(),
    );
    canon_a == canon_b
}

/// Build the `options` payload for `create_index` by stripping out
/// the fields that aren't options (`name` is its own argument, `key`
/// is its own argument, `v` is server-set). Everything else
/// (`unique`, `sparse`, `expireAfterSeconds`, …) rides along.
fn def_options_only(def: &Value) -> Value {
    let Value::Object(obj) = def else {
        return serde_json::json!({});
    };
    let mut out = serde_json::Map::new();
    for (k, v) in obj {
        if k == "name" || k == "key" || k == "v" {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ix(name: &str, key: Value) -> Value {
        json!({"name": name, "key": key, "v": 2})
    }

    #[test]
    fn diff_id_index_is_filtered_from_regular_arrays() {
        let plan = diff_for_dataset(
            &[ix("_id_", json!({"_id": 1}))],
            &[],
            &[ix("_id_", json!({"_id": 1}))],
            &[],
        );
        assert!(plan.drop_regular.is_empty());
        assert!(plan.create_regular.is_empty());
    }

    #[test]
    fn diff_local_only_index_is_created() {
        let plan = diff_for_dataset(
            &[ix("ix_vendor_id", json!({"vendor_id": 1}))],
            &[],
            &[],
            &[],
        );
        assert!(plan.drop_regular.is_empty());
        assert_eq!(plan.create_regular.len(), 1);
        assert_eq!(
            plan.create_regular[0].get("name").and_then(|v| v.as_str()),
            Some("ix_vendor_id")
        );
    }

    #[test]
    fn diff_remote_only_index_is_dropped() {
        let plan = diff_for_dataset(
            &[],
            &[],
            &[ix("ix_orphan", json!({"orphan": 1}))],
            &[],
        );
        assert_eq!(plan.drop_regular, vec!["ix_orphan".to_string()]);
        assert!(plan.create_regular.is_empty());
    }

    #[test]
    fn diff_changed_def_produces_drop_and_create() {
        // Same name, different key spec → drop the old, create the new.
        let plan = diff_for_dataset(
            &[ix("ix_x", json!({"x": -1}))],
            &[],
            &[ix("ix_x", json!({"x": 1}))],
            &[],
        );
        assert_eq!(plan.drop_regular, vec!["ix_x".to_string()]);
        assert_eq!(plan.create_regular.len(), 1);
    }

    #[test]
    fn diff_identical_def_is_a_noop_even_if_v_differs() {
        // `v` is server-set; differing `v` should not trigger churn.
        let local = json!({"name": "ix_y", "key": {"y": 1}});
        let remote = json!({"name": "ix_y", "key": {"y": 1}, "v": 2});
        let plan = diff_for_dataset(&[local], &[], &[remote], &[]);
        assert!(plan.drop_regular.is_empty(), "should not drop on v-only diff");
        assert!(plan.create_regular.is_empty(), "should not create on v-only diff");
    }

    #[test]
    fn diff_search_index_create_drop_pair() {
        let local = json!({"name": "search1", "mappings": {"dynamic": true}});
        let remote_other = json!({"name": "search2", "mappings": {"dynamic": false}});
        let plan = diff_for_dataset(&[], &[local], &[], &[remote_other]);
        assert_eq!(plan.drop_search, vec!["search2".to_string()]);
        assert_eq!(plan.create_search.len(), 1);
        assert_eq!(
            plan.create_search[0].get("name").and_then(|v| v.as_str()),
            Some("search1")
        );
    }

    #[test]
    fn options_strip_keeps_user_options() {
        let def = json!({
            "name": "ix_z",
            "key": {"z": 1},
            "v": 2,
            "unique": true,
            "sparse": false,
        });
        let opts = def_options_only(&def);
        let obj = opts.as_object().unwrap();
        assert!(!obj.contains_key("name"));
        assert!(!obj.contains_key("key"));
        assert!(!obj.contains_key("v"));
        assert_eq!(obj.get("unique"), Some(&json!(true)));
        assert_eq!(obj.get("sparse"), Some(&json!(false)));
    }

    #[test]
    fn diff_remote_only_not_dropped_without_mirror() {
        // Additive (deploy default): an index that exists only on the
        // target is left in place — NOT pruned.
        let plan =
            diff_indexes(&[], &[], &[ix("ix_orphan", json!({"orphan": 1}))], &[], false);
        assert!(
            plan.drop_regular.is_empty(),
            "remote-only index must survive without mirror: {plan:?}"
        );
        assert!(plan.create_regular.is_empty());
    }

    #[test]
    fn diff_remote_only_dropped_with_mirror() {
        let plan = diff_indexes(&[], &[], &[ix("ix_orphan", json!({"orphan": 1}))], &[], true);
        assert_eq!(plan.drop_regular, vec!["ix_orphan".to_string()]);
    }

    #[test]
    fn diff_changed_def_drops_even_without_mirror() {
        // A changed definition is always drop+create (no in-place update),
        // independent of mirror.
        let plan = diff_indexes(
            &[ix("ix_x", json!({"x": -1}))],
            &[],
            &[ix("ix_x", json!({"x": 1}))],
            &[],
            false,
        );
        assert_eq!(plan.drop_regular, vec!["ix_x".to_string()]);
        assert_eq!(plan.create_regular.len(), 1);
    }

    #[test]
    fn diff_remote_only_search_not_dropped_without_mirror() {
        let remote = json!({"name": "s_orphan", "mappings": {"dynamic": true}});
        let plan = diff_indexes(&[], &[], &[], &[remote], false);
        assert!(
            plan.drop_search.is_empty(),
            "remote-only search index must survive without mirror: {plan:?}"
        );
    }
}
