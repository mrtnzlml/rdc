use super::common::{apply_pull_action, decide_pull_action, record_object, PullAction, PullCtx};
use crate::api::{anyhow_has_status, DataStorageClient};
use crate::config::EnvConfig;
use crate::model::{Collection, IndexSet};
use crate::progress::ProgressLog;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use futures::stream::{StreamExt, TryStreamExt};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

/// Opaque listed state for MDH — the client handle plus the collection list.
/// We carry the client here because it's constructed from env_cfg + token,
/// which live in `run_drivers` scope.
pub struct MdhListed {
    pub client: DataStorageClient,
    pub collections: Vec<Collection>,
}

/// Phase 1: list MDH collections (or return an empty list if MDH is not
/// enabled on this cluster — 404 → quiet skip matching the 403 pattern).
pub async fn list(env_cfg: &EnvConfig, token: &str, progress: &Arc<ProgressLog>) -> Result<MdhListed> {
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
    progress: &Arc<ProgressLog>,
) -> Result<(usize, usize)> {
    let phase = progress.phase("pulling mdh");

    let MdhListed { client, collections } = listed;

    if collections.is_empty() {
        return Ok((0, 0));
    }

    let mut used: HashSet<String> = HashSet::new();
    let mut conflicts = 0usize;

    let mut dir_created = false;

    // === Sub-phase A: assign slugs + write collection.json. Build per-collection
    //            dataset_dir map for use in sub-phase C.
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

        let coll_path = dataset_dir.join("collection.json");
        let mut coll_proposed = serde_json::to_vec_pretty(&c).context("serializing collection")?;
        coll_proposed.push(b'\n');
        let coll_base = ctx
            .lockfile
            .objects
            .get("mdh_collections")
            .and_then(|m| m.get(&slug))
            .and_then(|e| e.content_hash.clone());
        let (c_action, c_remote_hash) =
            decide_pull_action(&coll_path, coll_base.as_deref(), &coll_proposed)?;
        if c_action == PullAction::Conflict {
            conflicts += 1;
        }
        let c_recorded = apply_pull_action(c_action, &coll_path, &coll_proposed, c_remote_hash, ctx.interactive, progress, ctx.paths.env(), coll_base.as_deref())?;
        record_object(
            ctx.lockfile,
            "mdh_collections",
            &slug,
            0,
            None,
            None,
            Some(c_recorded),
        );

        dataset_dirs.push((slug, dataset_dir, c));
    }

    // === Sub-phase B: concurrent index fetches per collection (regular +
    //            search). Bounded fan-out (see common::PULL_FANOUT).
    let client_ref = &client;
    let progress_inner = progress.clone();
    let fetched: Vec<(String, IndexSet)> = futures::stream::iter(
        dataset_dirs.iter().map(|(slug, _, c)| (slug.clone(), c.name.clone()))
    )
    .map(|(slug, name)| {
        let p = progress_inner.clone();
        async move {
            let regular = client_ref.list_indexes(&name, Some(p.clone())).await
                .with_context(|| format!("listing indexes for '{name}'"))?;
            let search = client_ref.list_search_indexes(&name, Some(p.clone())).await
                .with_context(|| format!("listing search indexes for '{name}'"))?;
            Ok::<_, anyhow::Error>((slug, IndexSet { regular, search }))
        }
    })
    .buffer_unordered(crate::cli::pull::common::PULL_FANOUT)
    .try_collect()
    .await?;
    let by_slug: std::collections::HashMap<String, IndexSet> = fetched.into_iter().collect();

    // === Sub-phase C: per-collection indexes.json write decision (sequential
    //            because we mutate ctx.lockfile + counts).
    for (slug, dataset_dir, c) in &dataset_dirs {
        let Some(index_set) = by_slug.get(slug) else { continue };

        let sp = phase.item(&c.name);
        let ix_path = dataset_dir.join("indexes.json");
        let mut ix_proposed = serde_json::to_vec_pretty(index_set).context("serializing index set")?;
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
        let i_recorded = apply_pull_action(i_action, &ix_path, &ix_proposed, i_remote_hash, ctx.interactive, progress, ctx.paths.env(), ix_base.as_deref())?;
        record_object(
            ctx.lockfile,
            "mdh_indexes",
            slug,
            0,
            None,
            None,
            Some(i_recorded),
        );
        sp.finish_ok("");
    }

    Ok((dataset_dirs.len(), conflicts))
}
