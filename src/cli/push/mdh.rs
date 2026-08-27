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
use crate::state::{Lockfile, ObjectEntry, content_hash};
use anyhow::{Context, Result, anyhow};
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

/// Maximum time to wait for a just-created index to REGISTER in the
/// remote listing. Both index kinds appear in their list almost
/// immediately after a successful create (regular in-progress builds
/// are listed; Atlas Search registers before the background build
/// finishes) — so an index still absent after this window almost
/// certainly failed to build. The canonical failure: a UNIQUE index
/// over data that already contains duplicate key values — the Data
/// Storage API ACKs the create, the async build fails, and no error
/// ever reaches the creator. That case is normally caught upfront by
/// [`preflight_doomed_unique_creates`] (no create issued, no wait);
/// this verification remains as the backstop for every other silent
/// build failure.
const CREATE_MATERIALIZE_TIMEOUT: Duration = Duration::from_secs(10);

/// Divisor applied to every wall-clock wait in this module when the client
/// talks to a loopback mock (see [`crate::api::is_loopback_base`]). The
/// waits above model the Data Storage service building an index
/// asynchronously; a mock answers instantly and either has the index or
/// never will, so against one the ceilings are pure sleep — 2 x
/// `CREATE_MATERIALIZE_TIMEOUT` was 20 s of `tests/cli_sync.rs`'s 70 s.
///
/// A divisor, not a separate set of constants, so the *ratio* between poll
/// interval and ceiling is preserved: a mock-backed wait still polls ~20
/// times before expiring, exercising the same loop and the same
/// timeout-expiry branch as production.
const LOCAL_MOCK_SPEEDUP: u32 = 40;

/// Scale a production wait for the client it will be spent against.
fn scaled(client: &DataStorageClient, d: Duration) -> Duration {
    if client.is_loopback() { d / LOCAL_MOCK_SPEEDUP } else { d }
}

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
    paths: &crate::paths::Paths,
    allow_deletes: bool,
    interactive: bool,
    progress: &Arc<Log>,
) -> Result<usize> {
    let local_raw = std::fs::read(indexes_path)
        .with_context(|| format!("reading {}", indexes_path.display()))?;
    let local_set: IndexSet = serde_json::from_slice(&local_raw)
        .with_context(|| format!("parsing {}", indexes_path.display()))?;

    // Base leg of the 3-way diff: the last-synced index set, written to the
    // base cache by the MDH pull driver. Absent (None) on a never-synced
    // dataset → empty set → no removals can be proven (strictly safe).
    let base_set: IndexSet = match crate::state::base_cache::read(paths, indexes_path)? {
        Some(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing base cache for {}", indexes_path.display()))?,
        None => IndexSet::default(),
    };

    // Fetch the live remote state directly so an admin's UI-added indexes are
    // visible to the diff (and, being absent from base, preserved).
    let remote_regular = client
        .list_indexes(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("listing regular indexes for '{collection_name}'"))?;
    let remote_search = client
        .list_search_indexes(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("listing search indexes for '{collection_name}'"))?;

    // Reshape the raw remote search-index list entries to the same canonical
    // {name, mappings, analyzers?} form the pull writes locally, so the diff
    // compares like-with-like. Without this, every search index looks "changed"
    // (local normalized vs remote raw) and is needlessly dropped+recreated on
    // every sync.
    let remote_search_norm: Vec<Value> = remote_search
        .iter()
        .filter_map(|entry| {
            let norm = crate::snapshot::codec::normalize_search_index(entry);
            if norm.is_none() {
                progress.event(
                    Action::Warn,
                    &format!("mdh/{slug} skipping un-normalizable remote search index entry: {entry}"),
                );
            }
            norm
        })
        .collect();

    let diff = diff_indexes_3way(
        &base_set.regular,
        &base_set.search,
        &local_set.regular,
        &local_set.search,
        &remote_regular,
        &remote_search_norm,
    );
    let mut plan = diff.plan;

    // Gate genuine user-removal drops behind --allow-deletes (mirrors the
    // global delete gate). Changed-def recreates in `plan` are NOT gated.
    let pending = diff.pending_regular_deletes.len() + diff.pending_search_deletes.len();
    let mut skipped = false;
    match classify_delete_gate(pending, allow_deletes, interactive) {
        DeleteGate::Proceed => {
            plan.drop_regular.extend(diff.pending_regular_deletes.iter().cloned());
            plan.drop_search.extend(diff.pending_search_deletes.iter().cloned());
        }
        DeleteGate::Bail => {
            anyhow::bail!(
                "{pending} MDH index(es) on '{collection_name}' marked for deletion but \
                 --allow-deletes was not passed. Re-run with --allow-deletes to authorise \
                 the destructive push, or restore {} to cancel.",
                indexes_path.display()
            );
        }
        DeleteGate::Prompt => {
            let proceed = prompt_confirm_index_drops(
                progress,
                collection_name,
                &diff.pending_regular_deletes,
                &diff.pending_search_deletes,
            )?;
            if proceed {
                plan.drop_regular.extend(diff.pending_regular_deletes.iter().cloned());
                plan.drop_search.extend(diff.pending_search_deletes.iter().cloned());
            } else {
                skipped = true;
                progress.event(
                    Action::Skip,
                    &format!("mdh/{slug} {pending} index deletion(s) skipped"),
                );
            }
        }
    }

    // A unique index over data that already contains duplicate key values is
    // doomed: the API ACKs the create, the async build fails, and the index
    // silently never appears — re-attempted on every subsequent sync (a
    // futile write + a 10s materialization wait each run). Detect that data
    // state upfront with a cheap aggregation and drop the doomed creates
    // from the plan; the dataset stays not-fully-applied, so the create
    // retries automatically once the data has been deduplicated.
    let doomed =
        preflight_doomed_unique_creates(client, collection_name, slug, &mut plan, progress)
            .await;

    let ops = apply_diff(client, collection_name, slug, &plan, progress).await?;

    // The Data Storage API ACKs creates and builds asynchronously; a failed
    // build silently never appears. Verify every create registered, warn for
    // the ones that didn't, and exclude them from the op count — they
    // changed nothing.
    let missing = verify_creates_materialized(
        client,
        collection_name,
        slug,
        &plan,
        scaled(client, CREATE_MATERIALIZE_TIMEOUT),
        progress,
    )
    .await?;
    let ops = ops.saturating_sub(missing);

    // Refresh the lockfile content_hash AND the base cache only when the push
    // fully reconciled remote to local (no skipped removals, every create
    // materialized, no doomed unique creates withheld). Refreshing on a
    // skipped removal would make the next sync's `local_hash == base` gate
    // skip the dataset and silently forget the pending removal; refreshing on
    // a vanished or withheld create would record an index the remote does not
    // have. Writing the base cache here restores the cache↔lockfile hash
    // invariant (the old code refreshed the lockfile but never the base
    // cache).
    let fully_applied = ops > 0 && !skipped && missing == 0 && doomed == 0;
    if fully_applied {
        let hash = content_hash(&local_raw, &crate::state::Lockfile::default());
        let map = lockfile
            .objects
            .entry("mdh_indexes".to_string())
            .or_default();
        map.insert(
            slug.to_string(),
            ObjectEntry {
                id: 0,
                modified_at: None,
                modified_by: None,
                content_hash: Some(hash),
                secrets_hash: None,
            },
        );
        crate::state::base_cache::write(paths, indexes_path, &local_raw)
            .with_context(|| format!("writing base cache for mdh/{slug}"))?;
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
        progress.event(
            Action::Delete,
            &format!("mdh/{slug} regular index '{name}'"),
        );
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
            .create_index(
                collection_name,
                name,
                keys,
                &options,
                Some(progress.clone()),
            )
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
            .create_search_index(
                collection_name,
                name,
                mappings,
                &analyzers,
                Some(progress.clone()),
            )
            .await
            .with_context(|| format!("creating search index '{name}' on '{collection_name}'"))?;
        progress.event(Action::Post, &format!("mdh/{slug} search index '{name}'"));
        ops += 1;
    }
    Ok(ops)
}

/// Poll the remote listings until every index just created by
/// [`apply_diff`] has REGISTERED, or `timeout` expires. Returns how many
/// never appeared, warning for each: the Data Storage API ACKs
/// `indexes/create` / `search_indexes/create` and builds asynchronously,
/// and a failed build vanishes without any error surfacing to the
/// creator — most commonly a UNIQUE index whose keys are violated by
/// duplicate values already in the collection. Callers subtract the
/// count from their op tally (a vanished create changed nothing) and
/// treat the push as not-fully-applied so the retry stays armed for
/// after the user fixes the data.
pub(crate) async fn verify_creates_materialized(
    client: &DataStorageClient,
    collection_name: &str,
    slug: &str,
    plan: &DiffPlan,
    timeout: Duration,
    progress: &Arc<Log>,
) -> Result<usize> {
    let name_of = |def: &Value| def.get("name").and_then(|n| n.as_str()).map(str::to_string);
    let want_regular: Vec<Value> = plan.create_regular.clone();
    let want_search: Vec<String> = plan.create_search.iter().filter_map(name_of).collect();
    if want_regular.is_empty() && want_search.is_empty() {
        return Ok(0);
    }

    let listed_names = |list: &[Value]| -> BTreeSet<String> {
        list.iter()
            .filter_map(|ix| ix.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .collect()
    };

    let start = Instant::now();
    loop {
        let mut missing_regular: Vec<&Value> = Vec::new();
        if !want_regular.is_empty() {
            let have = listed_names(
                &client
                    .list_indexes(collection_name, Some(progress.clone()))
                    .await
                    .with_context(|| {
                        format!("verifying created indexes on '{collection_name}'")
                    })?,
            );
            missing_regular = want_regular
                .iter()
                .filter(|def| name_of(def).is_some_and(|n| !have.contains(&n)))
                .collect();
        }
        let mut missing_search: Vec<&String> = Vec::new();
        if !want_search.is_empty() {
            let have = listed_names(
                &client
                    .list_search_indexes(collection_name, Some(progress.clone()))
                    .await
                    .with_context(|| {
                        format!("verifying created search indexes on '{collection_name}'")
                    })?,
            );
            missing_search = want_search.iter().filter(|n| !have.contains(*n)).collect();
        }

        if missing_regular.is_empty() && missing_search.is_empty() {
            return Ok(0);
        }
        if start.elapsed() >= timeout {
            for def in &missing_regular {
                let name = name_of(def).unwrap_or_default();
                let unique_hint = if def.get("unique").and_then(|u| u.as_bool()).unwrap_or(false)
                {
                    " It is a UNIQUE index — the collection most likely contains duplicate \
                     values for its key(s); deduplicate the data and re-run."
                } else {
                    ""
                };
                progress.event(
                    Action::Warn,
                    &format!(
                        "mdh/{slug} regular index '{name}' on '{collection_name}' was accepted \
                         but never materialized (the async build likely failed).{unique_hint}"
                    ),
                );
            }
            for name in &missing_search {
                progress.event(
                    Action::Warn,
                    &format!(
                        "mdh/{slug} search index '{name}' on '{collection_name}' was accepted \
                         but never materialized (the async build likely failed)."
                    ),
                );
            }
            return Ok(missing_regular.len() + missing_search.len());
        }
        tokio::time::sleep(scaled(client, DROP_POLL_INTERVAL)).await;
    }
}

/// Build the duplicate-key detection pipeline for a unique regular-index
/// definition, or `None` when the definition isn't eligible for the
/// preflight. Eligible: `unique: true`, plain field-name keys, and no
/// `sparse` / `partialFilterExpression` (those scope uniqueness to a
/// subset of documents the plain `$group` below doesn't model — such
/// creates go straight to the attempt-and-verify path).
///
/// The pipeline groups every document by the index's key fields and
/// counts groups with more than one member:
/// `[{$group: {_id: {k0: "$f0", …}, n: {$sum: 1}}}, {$match: {n: {$gt: 1}}},
///   {$count: "dupGroups"}]`
/// — an empty result means the data can satisfy the unique constraint.
/// Group-`_id` subfields are positional (`k0`, `k1`, …) because dotted
/// index paths (`id.poId`) are not valid document keys there.
pub(crate) fn unique_dup_key_pipeline(def: &Value) -> Option<Value> {
    if !def.get("unique").and_then(|u| u.as_bool()).unwrap_or(false) {
        return None;
    }
    if def.get("sparse").and_then(|s| s.as_bool()).unwrap_or(false)
        || def.get("partialFilterExpression").is_some()
    {
        return None;
    }
    let key = def.get("key")?.as_object()?;
    if key.is_empty() {
        return None;
    }
    let mut group_id = serde_json::Map::new();
    for (i, field) in key.keys().enumerate() {
        // `$`-prefixed key names are not real field paths (e.g. the `$**`
        // wildcard spec) — and can't be unique anyway. Bail defensively.
        if field.starts_with('$') {
            return None;
        }
        group_id.insert(format!("k{i}"), Value::String(format!("${field}")));
    }
    Some(serde_json::json!([
        { "$group": { "_id": group_id, "n": { "$sum": 1 } } },
        { "$match": { "n": { "$gt": 1 } } },
        { "$count": "dupGroups" }
    ]))
}

/// Remove from `plan` every unique regular-index create whose key is
/// already violated by duplicate values in the collection — the async
/// build would inevitably fail (silently), so attempting it is a futile
/// remote write plus a full materialization wait, repeated every sync
/// until the data is fixed. A withheld create also cancels its paired
/// same-name drop, so a changed-definition recreate never destroys the
/// existing index only to fail rebuilding it.
///
/// Returns the number of creates withheld; the caller treats any
/// non-zero count as not-fully-applied so the dataset is re-examined
/// (and the create re-attempted) on the next sync — statelessly
/// self-healing once the data has been deduplicated. An aggregation
/// failure (older Data Storage without `data/aggregate`, oversized
/// collection, …) leaves the create in the plan: the attempt-and-verify
/// path handles it exactly as before.
pub(crate) async fn preflight_doomed_unique_creates(
    client: &DataStorageClient,
    collection_name: &str,
    slug: &str,
    plan: &mut DiffPlan,
    progress: &Arc<Log>,
) -> usize {
    let mut withheld: Vec<String> = Vec::new();
    for def in &plan.create_regular {
        let Some(name) = def.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(pipeline) = unique_dup_key_pipeline(def) else {
            continue;
        };
        let dup_groups = match client
            .aggregate(collection_name, &pipeline, Some(progress.clone()))
            .await
        {
            Ok(result) => result
                .first()
                .and_then(|doc| doc.get("dupGroups"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0),
            // Preflight is best-effort: on any aggregation failure fall
            // through to the normal create + materialization verify.
            Err(_) => continue,
        };
        if dup_groups == 0 {
            continue;
        }
        progress.event(
            Action::Warn,
            &format!(
                "mdh/{slug} withholding unique index '{name}' on '{collection_name}': the \
                 collection contains {dup_groups} duplicate key group(s) its build would \
                 fail on; deduplicate the data and re-run sync"
            ),
        );
        withheld.push(name.to_string());
    }
    if withheld.is_empty() {
        return 0;
    }
    plan.create_regular.retain(|def| {
        def.get("name")
            .and_then(|v| v.as_str())
            .is_none_or(|n| !withheld.iter().any(|w| w == n))
    });
    // Keep the existing definition alive rather than dropping it for a
    // recreate that cannot build.
    plan.drop_regular.retain(|n| !withheld.contains(n));
    withheld.len()
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
    let timeout = scaled(client, REGULAR_DROP_TIMEOUT);
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
        if start.elapsed() >= timeout {
            return Err(anyhow!(
                "timed out after {:?} waiting for regular index '{}' on '{}' to drop",
                timeout,
                index_name,
                collection
            ));
        }
        tokio::time::sleep(scaled(client, DROP_POLL_INTERVAL)).await;
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
    let timeout = scaled(client, SEARCH_DROP_TIMEOUT);
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
        if start.elapsed() >= timeout {
            return Err(anyhow!(
                "timed out after {:?} waiting for search index '{}' on '{}' to drop",
                timeout,
                index_name,
                collection
            ));
        }
        tokio::time::sleep(scaled(client, DROP_POLL_INTERVAL)).await;
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

/// Build a name→def map, optionally filtering the implicit `_id_` regular
/// index (server-managed, can't be dropped). Used by the 3-way base-aware diff.
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

/// Outcome of gating index-deletion (pure; mirrors the global delete gate
/// `crate::cli::push::deletes::confirm_or_refuse`).
#[derive(Debug, PartialEq)]
pub(crate) enum DeleteGate {
    /// Apply the pending deletes (nothing gated, or `--allow-deletes` set).
    Proceed,
    /// Non-interactive without `--allow-deletes`: refuse the destructive push.
    Bail,
    /// Interactive without `--allow-deletes`: caller must prompt [y/N].
    Prompt,
}

/// Decide how to treat `pending` user-removal index drops, mirroring rdc's
/// global delete gate: `--allow-deletes` ⇒ proceed; else non-TTY ⇒ bail;
/// else (TTY) ⇒ prompt. With nothing pending, "proceed" is a no-op.
pub(crate) fn classify_delete_gate(
    pending: usize,
    allow_deletes: bool,
    interactive: bool,
) -> DeleteGate {
    if pending == 0 || allow_deletes {
        return DeleteGate::Proceed;
    }
    if !interactive {
        return DeleteGate::Bail;
    }
    DeleteGate::Prompt
}

/// Interactive [y/N] confirmation for dropping remote MDH indexes that are no
/// longer present locally. Returns `true` to proceed with the drops.
fn prompt_confirm_index_drops(
    progress: &Arc<Log>,
    collection_name: &str,
    pending_regular: &[String],
    pending_search: &[String],
) -> Result<bool> {
    progress.with_prompt(|| -> Result<bool> {
        use std::io::Write;
        let n = pending_regular.len() + pending_search.len();
        eprintln!();
        eprintln!(
            "The following {n} MDH index(es) on '{collection_name}' would be DROPPED \
             (no longer present locally):"
        );
        for name in pending_regular {
            eprintln!("  - regular index '{name}'");
        }
        for name in pending_search {
            eprintln!("  - search index '{name}'");
        }
        eprint!("Proceed with the drop(s)? [y/N] ");
        std::io::stderr().flush().ok();
        let ans = crate::cli::stdin_coord::read_line_coordinated()?
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        Ok(ans == "y" || ans == "yes")
    })
}

/// Result of the base-aware (3-way) within-env index diff. `plan` carries
/// creates and changed-definition drop+recreate pairs (always applied);
/// `pending_*_deletes` carries genuine user removals (in base, gone from
/// local, still on remote) which are GATED behind `--allow-deletes`.
#[derive(Debug, Default)]
pub(crate) struct ThreeWayDiff {
    pub plan: DiffPlan,
    pub pending_regular_deletes: Vec<String>,
    pub pending_search_deletes: Vec<String>,
}

/// Base-aware diff for the within-env push driver. Distinguishes a *user
/// removal* (index was in the last-synced base, removed locally, still on
/// remote) from an *admin addition* (index appeared on remote, never in base
/// or local). The former is a gated pending delete; the latter is left
/// untouched.
pub(crate) fn diff_indexes_3way(
    base_regular: &[Value],
    base_search: &[Value],
    local_regular: &[Value],
    local_search: &[Value],
    remote_regular: &[Value],
    remote_search: &[Value],
) -> ThreeWayDiff {
    let mut out = ThreeWayDiff::default();
    diff_one_kind(
        base_regular,
        local_regular,
        remote_regular,
        true, // filter the implicit _id_ regular index
        &mut out.plan.drop_regular,
        &mut out.plan.create_regular,
        &mut out.pending_regular_deletes,
    );
    diff_one_kind(
        base_search,
        local_search,
        remote_search,
        false,
        &mut out.plan.drop_search,
        &mut out.plan.create_search,
        &mut out.pending_search_deletes,
    );
    out
}

/// Core 3-way classification for one index kind (regular or search).
/// `drops`/`creates` receive always-applied changed-def recreate pairs and
/// local-only creates; `pending_deletes` receives gated user removals.
fn diff_one_kind(
    base: &[Value],
    local: &[Value],
    remote: &[Value],
    filter_id: bool,
    drops: &mut Vec<String>,
    creates: &mut Vec<Value>,
    pending_deletes: &mut Vec<String>,
) {
    let base_map = index_by_name(base, filter_id);
    let local_map = index_by_name(local, filter_id);
    let remote_map = index_by_name(remote, filter_id);

    // Creates + changed-def recreate, driven by local (BTreeMap → sorted,
    // deterministic).
    for (name, local_def) in &local_map {
        match remote_map.get(name) {
            None => creates.push((*local_def).clone()), // local-only
            Some(remote_def) => {
                if !defs_equivalent(local_def, remote_def) {
                    drops.push(name.clone());
                    creates.push((*local_def).clone());
                }
            }
        }
    }
    // Removals, driven by remote-only entries.
    for name in remote_map.keys() {
        if local_map.contains_key(name) {
            continue; // present locally → handled above
        }
        // Remote-only: a genuine user removal ONLY if it was in the base.
        // Not in base ⇒ admin-added ⇒ survive (never dropped).
        if base_map.contains_key(name) {
            pending_deletes.push(name.clone());
        }
    }
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
        &crate::state::Lockfile::default(),
    );
    let canon_b = crate::snapshot::noise::canonicalize_for_hash(
        &serde_json::to_vec(&b).unwrap_or_default(),
        &crate::state::Lockfile::default(),
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

    // --- 3-way (base-aware) diff: the within-env safe-push semantics ---

    #[test]
    fn three_way_admin_added_remote_only_survives() {
        // Admin added ix_admin remotely; it's not in base and not in local.
        // It must NOT be dropped and must NOT become a pending delete.
        let base = vec![ix("ix_keep", json!({"k": 1}))];
        let local = vec![ix("ix_keep", json!({"k": 1}))];
        let remote = vec![ix("ix_keep", json!({"k": 1})), ix("ix_admin", json!({"a": 1}))];
        let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
        assert!(d.plan.drop_regular.is_empty(), "{d:?}");
        assert!(d.pending_regular_deletes.is_empty(), "{d:?}");
        assert!(d.plan.create_regular.is_empty(), "{d:?}");
    }

    #[test]
    fn three_way_user_removed_index_is_pending_delete() {
        // ix_gone was in base, removed from local, still on remote -> pending.
        let base = vec![ix("ix_keep", json!({"k": 1})), ix("ix_gone", json!({"g": 1}))];
        let local = vec![ix("ix_keep", json!({"k": 1}))];
        let remote = vec![ix("ix_keep", json!({"k": 1})), ix("ix_gone", json!({"g": 1}))];
        let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
        assert_eq!(d.pending_regular_deletes, vec!["ix_gone".to_string()]);
        assert!(d.plan.drop_regular.is_empty(), "pending != plan-drop: {d:?}");
        assert!(d.plan.create_regular.is_empty());
    }

    #[test]
    fn three_way_local_only_index_is_created() {
        let base = vec![];
        let local = vec![ix("ix_new", json!({"n": 1}))];
        let remote = vec![];
        let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
        assert_eq!(d.plan.create_regular.len(), 1);
        assert!(d.plan.drop_regular.is_empty());
        assert!(d.pending_regular_deletes.is_empty());
    }

    #[test]
    fn three_way_changed_def_is_drop_and_create_not_pending() {
        // Same name, diverging def -> always drop+recreate, never gated.
        let base = vec![ix("ix_x", json!({"x": 1}))];
        let local = vec![ix("ix_x", json!({"x": -1}))];
        let remote = vec![ix("ix_x", json!({"x": 1}))];
        let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
        assert_eq!(d.plan.drop_regular, vec!["ix_x".to_string()]);
        assert_eq!(d.plan.create_regular.len(), 1);
        assert!(d.pending_regular_deletes.is_empty());
    }

    #[test]
    fn three_way_no_base_never_pends_deletes() {
        // No base (empty) + a remote-only index not in local -> can't prove a
        // user removal, so NO pending delete (strictly-safe fallback). A local
        // create still happens.
        let base = vec![];
        let local = vec![ix("ix_new", json!({"n": 1}))];
        let remote = vec![ix("ix_admin", json!({"a": 1}))];
        let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
        assert_eq!(d.plan.create_regular.len(), 1, "local-only still created: {d:?}");
        assert!(d.pending_regular_deletes.is_empty(), "no base => no pending: {d:?}");
        assert!(d.plan.drop_regular.is_empty());
    }

    #[test]
    fn three_way_id_index_filtered_on_all_sides() {
        let base = vec![ix("_id_", json!({"_id": 1}))];
        let local = vec![ix("_id_", json!({"_id": 1}))];
        let remote = vec![ix("_id_", json!({"_id": 1}))];
        let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
        assert!(d.plan.drop_regular.is_empty());
        assert!(d.plan.create_regular.is_empty());
        assert!(d.pending_regular_deletes.is_empty());
    }

    #[test]
    fn three_way_v_only_diff_is_noop() {
        let base = vec![json!({"name": "ix_y", "key": {"y": 1}})];
        let local = vec![json!({"name": "ix_y", "key": {"y": 1}})];
        let remote = vec![json!({"name": "ix_y", "key": {"y": 1}, "v": 2})];
        let d = diff_indexes_3way(&base, &[], &local, &[], &remote, &[]);
        assert!(d.plan.drop_regular.is_empty(), "{d:?}");
        assert!(d.plan.create_regular.is_empty(), "{d:?}");
        assert!(d.pending_regular_deletes.is_empty());
    }

    #[test]
    fn three_way_search_user_removed_is_pending() {
        let s = |name: &str, dynamic: bool| json!({"name": name, "mappings": {"dynamic": dynamic}});
        let base = vec![s("sx_gone", true)];
        let local: Vec<serde_json::Value> = vec![];
        let remote = vec![s("sx_gone", true)];
        let d = diff_indexes_3way(&[], &base, &[], &local, &[], &remote);
        assert_eq!(d.pending_search_deletes, vec!["sx_gone".to_string()]);
        assert!(d.plan.drop_search.is_empty());
    }

    #[test]
    fn three_way_search_admin_added_survives() {
        let s = |name: &str, dynamic: bool| json!({"name": name, "mappings": {"dynamic": dynamic}});
        let d = diff_indexes_3way(&[], &[], &[], &[], &[], &[s("sx_admin", true)]);
        assert!(d.pending_search_deletes.is_empty(), "{d:?}");
        assert!(d.plan.drop_search.is_empty());
    }

    #[test]
    fn gate_no_pending_is_proceed() {
        assert_eq!(classify_delete_gate(0, false, false), DeleteGate::Proceed);
        assert_eq!(classify_delete_gate(0, false, true), DeleteGate::Proceed);
    }

    #[test]
    fn gate_allow_deletes_proceeds() {
        assert_eq!(classify_delete_gate(3, true, false), DeleteGate::Proceed);
        assert_eq!(classify_delete_gate(3, true, true), DeleteGate::Proceed);
    }

    #[test]
    fn gate_noninteractive_without_flag_bails() {
        assert_eq!(classify_delete_gate(1, false, false), DeleteGate::Bail);
    }

    #[test]
    fn gate_interactive_without_flag_prompts() {
        assert_eq!(classify_delete_gate(1, false, true), DeleteGate::Prompt);
    }

    // --- create-materialization verification -------------------------
    //
    // The Data Storage API ACKs `indexes/create` and builds asynchronously;
    // a failed build (e.g. a unique index over data that already contains
    // duplicate key values) vanishes WITHOUT any error ever reaching the
    // creator. `verify_creates_materialized` polls the list until every
    // just-created index appears or the timeout expires, so the push can
    // surface the silent failure instead of reporting success.

    fn test_log() -> Arc<crate::log::Log> {
        crate::log::Log::new(crate::cli::resolve::ColorMode::Plain)
    }

    #[tokio::test]
    async fn verify_reports_create_that_never_materializes() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        // The created index never appears in the listing.
        Mock::given(method("POST"))
            .and(path("/v1/indexes/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "",
                "result": [{ "v": 2, "name": "_id_", "key": { "_id": 1 } }]
            })))
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "tok".to_string()).unwrap();
        let plan = DiffPlan {
            create_regular: vec![json!({"name": "ix_u", "key": {"a": 1}, "unique": true})],
            ..Default::default()
        };
        let missing = verify_creates_materialized(
            &client,
            "vendors",
            "vendors",
            &plan,
            Duration::ZERO,
            &test_log(),
        )
        .await
        .expect("verification listing should succeed");
        assert_eq!(missing, 1, "the never-materializing create must be reported");
    }

    #[tokio::test]
    async fn verify_passes_when_created_index_appears() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/indexes/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "",
                "result": [
                    { "v": 2, "name": "_id_", "key": { "_id": 1 } },
                    { "v": 2, "name": "ix_u", "key": { "a": 1 }, "unique": true }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search_indexes/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "",
                "result": [{ "name": "sx", "latestDefinition": { "mappings": { "dynamic": true } } }]
            })))
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "tok".to_string()).unwrap();
        let plan = DiffPlan {
            create_regular: vec![json!({"name": "ix_u", "key": {"a": 1}, "unique": true})],
            create_search: vec![json!({"name": "sx", "mappings": {"dynamic": true}})],
            ..Default::default()
        };
        let missing = verify_creates_materialized(
            &client,
            "vendors",
            "vendors",
            &plan,
            Duration::ZERO,
            &test_log(),
        )
        .await
        .expect("verification listing should succeed");
        assert_eq!(missing, 0, "materialized creates must not be reported");
    }

    // --- unique-index duplicate-key preflight -------------------------
    //
    // A unique index over data that already contains duplicate key values
    // can never build: the API ACKs the create and the async build fails
    // silently, so every sync re-attempts the create and waits out the
    // full materialization timeout. The preflight detects the doomed data
    // state with one cheap aggregation and withholds the create instead.

    #[test]
    fn dup_pipeline_built_for_plain_unique_index() {
        let def = json!({
            "name": "vendors_unique_id",
            "key": { "id.erpAcct": 1, "id.erpName": 1, "id.vendorId": 1 },
            "unique": true
        });
        let p = unique_dup_key_pipeline(&def).expect("plain unique index is eligible");
        assert_eq!(
            p,
            json!([
                { "$group": { "_id": {
                    "k0": "$id.erpAcct", "k1": "$id.erpName", "k2": "$id.vendorId"
                }, "n": { "$sum": 1 } } },
                { "$match": { "n": { "$gt": 1 } } },
                { "$count": "dupGroups" }
            ])
        );
    }

    #[test]
    fn dup_pipeline_skips_non_unique_sparse_partial_and_wildcard() {
        let non_unique = json!({"name": "ix", "key": {"a": 1}});
        assert!(unique_dup_key_pipeline(&non_unique).is_none(), "not unique");

        let sparse = json!({"name": "ix", "key": {"a": 1}, "unique": true, "sparse": true});
        assert!(
            unique_dup_key_pipeline(&sparse).is_none(),
            "sparse uniqueness only covers docs bearing the fields"
        );

        let partial = json!({
            "name": "ix", "key": {"a": 1}, "unique": true,
            "partialFilterExpression": {"a": {"$exists": true}}
        });
        assert!(
            unique_dup_key_pipeline(&partial).is_none(),
            "partial uniqueness only covers matching docs"
        );

        let wildcard = json!({"name": "ix", "key": {"$**": 1}, "unique": true});
        assert!(unique_dup_key_pipeline(&wildcard).is_none(), "$-prefixed key is not a field");

        let empty_key = json!({"name": "ix", "key": {}, "unique": true});
        assert!(unique_dup_key_pipeline(&empty_key).is_none(), "empty key spec");
    }

    /// Duplicate data present → the doomed create is withheld (and its
    /// paired same-name drop cancelled, preserving the existing index);
    /// clean sibling creates stay in the plan.
    #[tokio::test]
    async fn preflight_withholds_doomed_unique_create_and_paired_drop() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .and(body_partial_json(json!({"collectionName": "PO_CANCELS"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "", "result": [{ "dupGroups": 9 }]
            })))
            .expect(1)
            .mount(&server)
            .await;
        // No create/drop mocks: any write reaching the server would 404.

        let client = DataStorageClient::new(server.uri(), "tok".to_string()).unwrap();
        let mut plan = DiffPlan {
            // Changed-def recreate pair for the doomed index…
            drop_regular: vec!["ix_u".to_string()],
            create_regular: vec![
                json!({"name": "ix_u", "key": {"a": 1}, "unique": true}),
                // …plus an untouched non-unique sibling create.
                json!({"name": "ix_plain", "key": {"b": 1}}),
            ],
            ..Default::default()
        };
        let doomed = preflight_doomed_unique_creates(
            &client,
            "PO_CANCELS",
            "po-cancels",
            &mut plan,
            &test_log(),
        )
        .await;
        assert_eq!(doomed, 1);
        assert_eq!(
            plan.create_regular,
            vec![json!({"name": "ix_plain", "key": {"b": 1}})],
            "only the doomed unique create is withheld"
        );
        assert!(
            plan.drop_regular.is_empty(),
            "the paired drop must be cancelled so the old index survives: {plan:?}"
        );
    }

    /// No duplicates → the plan is untouched and the create proceeds.
    #[tokio::test]
    async fn preflight_passes_clean_unique_create_through() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "", "result": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "tok".to_string()).unwrap();
        let mut plan = DiffPlan {
            create_regular: vec![json!({"name": "ix_u", "key": {"a": 1}, "unique": true})],
            ..Default::default()
        };
        let doomed = preflight_doomed_unique_creates(
            &client, "VENDORS", "vendors", &mut plan, &test_log(),
        )
        .await;
        assert_eq!(doomed, 0);
        assert_eq!(plan.create_regular.len(), 1, "clean create stays planned");
    }

    /// Aggregation unavailable (e.g. older Data Storage) → best-effort
    /// fallback: the create stays planned for the attempt-and-verify path.
    #[tokio::test]
    async fn preflight_falls_back_on_aggregate_failure() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = DataStorageClient::new(server.uri(), "tok".to_string()).unwrap();
        let mut plan = DiffPlan {
            drop_regular: vec!["ix_u".to_string()],
            create_regular: vec![json!({"name": "ix_u", "key": {"a": 1}, "unique": true})],
            ..Default::default()
        };
        let doomed = preflight_doomed_unique_creates(
            &client, "VENDORS", "vendors", &mut plan, &test_log(),
        )
        .await;
        assert_eq!(doomed, 0);
        assert_eq!(plan.create_regular.len(), 1);
        assert_eq!(plan.drop_regular.len(), 1);
    }

    /// Non-unique creates never trigger an aggregation at all.
    #[tokio::test]
    async fn preflight_makes_no_requests_for_non_unique_creates() {
        // No mocks mounted: any request would 404 — but a 404 only causes
        // fallback, so additionally assert zero requests were received.
        let server = wiremock::MockServer::start().await;
        let client = DataStorageClient::new(server.uri(), "tok".to_string()).unwrap();
        let mut plan = DiffPlan {
            create_regular: vec![json!({"name": "ix_plain", "key": {"b": 1}})],
            ..Default::default()
        };
        let doomed = preflight_doomed_unique_creates(
            &client, "VENDORS", "vendors", &mut plan, &test_log(),
        )
        .await;
        assert_eq!(doomed, 0);
        assert_eq!(plan.create_regular.len(), 1);
        assert!(
            server.received_requests().await.unwrap_or_default().is_empty(),
            "non-unique creates must not be preflighted"
        );
    }

    /// End-to-end through `push_dataset`: a doomed unique create must leave
    /// the lockfile and base cache untouched (dataset stays not-fully-applied
    /// so the create retries once the data is deduplicated), perform ZERO
    /// index writes, and report 0 ops. Any create/drop reaching the server
    /// would 404 and error the push — the mocks below only answer the reads.
    #[tokio::test]
    async fn push_dataset_withholds_doomed_create_and_keeps_retry_armed() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/indexes/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "",
                "result": [{ "v": 2, "name": "_id_", "key": { "_id": 1 } }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search_indexes/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "", "result": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": "ok", "message": "", "result": [{ "dupGroups": 2 }]
            })))
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let paths = crate::paths::Paths::for_env(dir.path(), "test");
        let dataset_dir = paths.dataset_dir("vendors");
        std::fs::create_dir_all(&dataset_dir).unwrap();
        let indexes_path = dataset_dir.join("indexes.json");
        std::fs::write(
            &indexes_path,
            serde_json::to_vec_pretty(&json!({
                "regular": [{ "key": { "id.vendorId": 1 }, "name": "ix_u", "unique": true }],
                "search": []
            }))
            .unwrap(),
        )
        .unwrap();

        let client = DataStorageClient::new(server.uri(), "tok".to_string()).unwrap();
        let mut lockfile = Lockfile::default();
        let ops = push_dataset(
            &client,
            &mut lockfile,
            "VENDORS",
            "vendors",
            &indexes_path,
            &paths,
            false,
            false,
            &test_log(),
        )
        .await
        .expect("withholding a doomed create is not an error");
        assert_eq!(ops, 0, "a withheld create performs no write ops");
        assert!(
            lockfile
                .objects
                .get("mdh_indexes")
                .and_then(|m| m.get("vendors"))
                .is_none(),
            "lockfile must stay unrefreshed so the next sync retries the create"
        );
        assert!(
            crate::state::base_cache::read(&paths, &indexes_path)
                .unwrap()
                .is_none(),
            "base cache must stay unwritten so the next sync retries the create"
        );
    }

    #[tokio::test]
    async fn verify_makes_no_requests_without_creates() {
        // No mocks mounted: any request would 404 and error the client —
        // an empty plan must return without touching the network.
        let server = wiremock::MockServer::start().await;
        let client = DataStorageClient::new(server.uri(), "tok".to_string()).unwrap();
        let missing = verify_creates_materialized(
            &client,
            "vendors",
            "vendors",
            &DiffPlan::default(),
            Duration::ZERO,
            &test_log(),
        )
        .await
        .expect("empty plan must verify trivially");
        assert_eq!(missing, 0);
        assert!(
            server.received_requests().await.unwrap_or_default().is_empty(),
            "no creates → no verification requests"
        );
    }
}
