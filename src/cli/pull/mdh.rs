use super::common::{apply_pull_action, decide_pull_action, record_object, PullAction, PullCtx};
use crate::api::{anyhow_has_status, DataStorageClient};
use crate::config::EnvConfig;
use crate::log::{Action, Log};
use crate::model::{Collection, IndexSet};
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use futures::stream::{StreamExt, TryStreamExt};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

/// Strip server-only fields from an index set so the user only sees /
/// round-trips the fields they can actually edit. Two flavors:
///
/// - **Regular indexes**: drop the implicit `_id_` (server-managed,
///   can't be dropped) and the `v` index-version field
///   (server-assigned). Other fields (`key`, `name`, `unique`,
///   `sparse`, …) round-trip 1:1 between list and create.
///
/// - **Search indexes**: the list response wraps the user-authored
///   `mappings` / `analyzers` inside a `latest_definition` envelope
///   and adds server-status fields (`type`, `status`, `queryable`,
///   `analyzer`, `search_analyzer`, `synonyms`). Normalise to the
///   shape the create body expects: `{name, mappings, analyzers?}`.
///   Without this normalisation, push would round-trip user edits
///   against a remote shape they never wrote, producing spurious
///   drop+create churn on every sync.
fn strip_server_managed(set: &IndexSet) -> IndexSet {
    let mut regular: Vec<Value> = set
        .regular
        .iter()
        .filter(|ix| {
            ix.get("name").and_then(|n| n.as_str()) != Some("_id_")
        })
        .cloned()
        .collect();
    for ix in regular.iter_mut() {
        if let Value::Object(obj) = ix {
            obj.shift_remove("v");
        }
    }
    let search: Vec<Value> = set
        .search
        .iter()
        .filter_map(normalize_search_index)
        .collect();
    IndexSet { regular, search }
}

/// Reshape a search-index list response to the create-body shape.
/// Returns `None` for entries that can't supply the minimum fields
/// (`name` and `mappings`) — defensive against future API drift.
fn normalize_search_index(remote: &Value) -> Option<Value> {
    let obj = remote.as_object()?;
    let name = obj.get("name")?.clone();
    let definition = obj.get("latest_definition").and_then(|v| v.as_object());
    let mappings = definition
        .and_then(|d| d.get("mappings"))
        .or_else(|| obj.get("mappings"))?
        .clone();
    let mut out = serde_json::Map::new();
    out.insert("name".to_string(), name);
    out.insert("mappings".to_string(), mappings);
    // Only include `analyzers` when the user actually configured them
    // (non-empty array). The default-empty case matches the create
    // body's optional shape and keeps the on-disk JSON minimal.
    let analyzers = definition
        .and_then(|d| d.get("analyzers"))
        .or_else(|| obj.get("analyzers"));
    if let Some(a) = analyzers {
        let non_empty = a.as_array().map(|arr| !arr.is_empty()).unwrap_or(true);
        if non_empty {
            out.insert("analyzers".to_string(), a.clone());
        }
    }
    Some(Value::Object(out))
}

/// Opaque listed state for MDH — the client handle plus the collection list.
/// We carry the client here because it's constructed from env_cfg + token,
/// which live in `run_drivers` scope.
pub struct MdhListed {
    pub client: DataStorageClient,
    pub collections: Vec<Collection>,
}

/// Phase 1: list MDH collections (or return an empty list if MDH is not
/// enabled on this cluster — 404 → quiet skip matching the 403 pattern).
pub async fn list(env_cfg: &EnvConfig, token: &str, progress: &Arc<Log>) -> Result<MdhListed> {
    let base = env_cfg.data_storage_base();
    let client = DataStorageClient::new(base, token.to_string())
        .context("constructing Data Storage client")?;

    let collections = match client.list_collections(Some(progress.clone())).await {
        Ok(c) => c,
        Err(e) if anyhow_has_status(&e, 404) => {
            // MDH not enabled on this cluster — quietly skip.
            Vec::new()
        }
        Err(e) => return Err(e.context("listing MDH collections")),
    };

    Ok(MdhListed { client, collections })
}

/// Phase 2: write listed collections + indexes to disk.
///
/// Per-collection regular + search index fetches are pipelined with
/// `buffer_unordered(N)` (per spec §16, default N=5) so a 10-dataset MDH
/// doesn't take 20 sequential round-trips.
///
/// `subset` selects which `(kind, slug)` pairs are written, with kind
/// `"mdh"` keyed by dataset slug; items outside the subset are skipped
/// silently (no fetch, no write). Returns `(collection_count, conflicts)`
/// of items written.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    listed: MdhListed,
    subset: &BTreeSet<(String, String)>,
    progress: &Arc<Log>,
) -> Result<(usize, usize)> {
    let MdhListed { client, collections } = listed;

    if collections.is_empty() {
        return Ok((0, 0));
    }

    let mut used: HashSet<String> = HashSet::new();
    let mut conflicts = 0usize;

    let mut dir_created = false;

    // === Sub-phase A: assign slugs, ensure dataset_dir exists, and run the
    //            one-shot migration for projects that pre-date the
    //            collection.json removal (delete the stale file, drop the
    //            redundant `mdh_collections` lockfile entry). The
    //            indexes.json write itself happens in sub-phase C after
    //            the parallel fetches.
    let mut dataset_dirs: Vec<(String, std::path::PathBuf, Collection)> = Vec::new();
    for c in collections {
        let slug = slugify_unique(&c.name, &used);
        used.insert(slug.clone());

        if !subset.contains(&("mdh".to_string(), slug.clone())) {
            continue;
        }

        if !dir_created {
            std::fs::create_dir_all(ctx.paths.mdh_dir())
                .with_context(|| format!("creating {}", ctx.paths.mdh_dir().display()))?;
            dir_created = true;
        }

        let dataset_dir = ctx.paths.dataset_dir(&slug);
        std::fs::create_dir_all(&dataset_dir)
            .with_context(|| format!("creating {}", dataset_dir.display()))?;

        // Migration: pre-2.x projects carry a `collection.json` next to
        // `indexes.json` plus an `mdh_collections.<slug>` lockfile entry.
        // The collection metadata is entirely server-managed (uuid,
        // options, idIndex) and offers no user-editable surface, so we
        // drop it. Cleanup is idempotent: subsequent syncs find nothing
        // to remove and stay quiet.
        let legacy_coll_path = dataset_dir.join("collection.json");
        if legacy_coll_path.exists() {
            std::fs::remove_file(&legacy_coll_path).with_context(|| {
                format!("removing legacy {}", legacy_coll_path.display())
            })?;
            progress.event(
                Action::Info,
                &format!("migrated mdh/{slug}: removed obsolete collection.json"),
            );
        }
        if let Some(map) = ctx.lockfile.objects.get_mut("mdh_collections")
            && map.remove(&slug).is_some() {
                progress.event(
                    Action::Info,
                    &format!("migrated mdh/{slug}: dropped mdh_collections lockfile entry"),
                );
            }

        dataset_dirs.push((slug.clone(), dataset_dir, c));
    }
    // Clean up the lockfile's `mdh_collections` key entirely if it ended
    // up empty after migration. Leaves the json clean for users grepping
    // the lockfile.
    if let Some(map) = ctx.lockfile.objects.get("mdh_collections")
        && map.is_empty() {
            ctx.lockfile.objects.remove("mdh_collections");
        }

    // === Sub-phase B: concurrent index fetches per collection (regular +
    //            search). Bounded fan-out (see common::PULL_FANOUT); the
    //            per-token rate limiter is the real throughput cap.
    let client_ref = &client;
    let total = dataset_dirs.len();
    if total == 0 {
        return Ok((0, conflicts));
    }
    let fetched_result: Result<Vec<(String, IndexSet)>> = futures::stream::iter(
        dataset_dirs.iter().map(|(slug, _, c)| (slug.clone(), c.name.clone()))
    )
    .map(|(slug, name)| {
        let progress = progress.clone();
        async move {
            let regular = client_ref.list_indexes(&name, Some(progress.clone())).await
                .with_context(|| format!("listing indexes for '{name}'"))?;
            let search = client_ref.list_search_indexes(&name, Some(progress.clone())).await
                .with_context(|| format!("listing search indexes for '{name}'"))?;
            Ok::<_, anyhow::Error>((slug, IndexSet { regular, search }))
        }
    })
    .buffer_unordered(crate::cli::pull::common::PULL_FANOUT)
    .try_collect()
    .await;
    let fetched = fetched_result?;
    progress.event(Action::Pull, &format!("mdh_indexes ({total} fetched)"));
    let by_slug: std::collections::HashMap<String, IndexSet> = fetched.into_iter().collect();

    // === Sub-phase C: per-collection indexes.json write decision (sequential
    //            because we mutate ctx.lockfile + counts). The set is
    //            stripped of server-managed fields (the implicit `_id_`
    //            regular index, the `v` index-version field) before
    //            serializing so the on-disk JSON contains only what the
    //            user can actually edit.
    for (slug, dataset_dir, _c) in &dataset_dirs {
        let Some(index_set) = by_slug.get(slug) else { continue };
        let trimmed = strip_server_managed(index_set);

        let ix_result: Result<()> = (|| {

        let ix_path = dataset_dir.join("indexes.json");
        let mut ix_proposed = serde_json::to_vec_pretty(&trimmed).context("serializing index set")?;
        ix_proposed.push(b'\n');
        let ix_base = ctx
            .lockfile
            .objects
            .get("mdh_indexes")
            .and_then(|m| m.get(slug))
            .and_then(|e| e.content_hash.clone());
        let (i_action, i_remote_hash) =
            decide_pull_action(&ix_path, ix_base.as_deref(), &ix_proposed)?;
        if i_action == PullAction::Conflict {
            conflicts += 1;
        }
        let i_recorded = apply_pull_action(i_action, &ix_path, &ix_proposed, i_remote_hash, ctx.interactive, progress, ctx.paths.env(), ix_base.as_deref(), Some(ctx.paths))?;
        record_object(
            ctx.lockfile,
            "mdh_indexes",
            slug,
            0,
            None,
            None,
            Some(i_recorded),
        );
        Ok(())
        })();
        ix_result?;
    }

    if !dataset_dirs.is_empty() {
        progress.event(Action::Pull, &format!("mdh_datasets ({} pulled)", dataset_dirs.len()));
    }

    Ok((dataset_dirs.len(), conflicts))
}
