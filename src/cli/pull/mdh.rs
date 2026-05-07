use super::common::{apply_pull_action, decide_pull_action, record_object, PullAction, PullCtx};
use crate::api::{anyhow_has_status, DataStorageClient};
use crate::config::EnvConfig;
use crate::model::IndexSet;
use crate::progress::KindProgress;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use futures::stream::{StreamExt, TryStreamExt};
use std::collections::HashSet;

/// Pulls Master Data Hub collections + indexes for `env_cfg`. The Data
/// Storage base URL is always derived from `env_cfg.api_base` (no separate
/// config field). On clusters without MDH the first call returns 404, in
/// which case we silently skip — same shape as the 403/permission skip
/// applied to other kinds.
///
/// Per-collection regular + search index fetches are pipelined with
/// `buffer_unordered(N)` (per spec §16, default N=5) so a 10-dataset MDH
/// doesn't take 20 sequential round-trips.
///
/// Returns `(collection_count, conflicts)`.
pub async fn pull(ctx: &mut PullCtx<'_>, env_cfg: &EnvConfig, token: &str, progress: &KindProgress) -> Result<(usize, usize)> {
    let base = env_cfg.data_storage_base();

    let client = DataStorageClient::new(base, token.to_string())
        .context("constructing Data Storage client")?;

    let collections = match client.list_collections(Some(progress)).await {
        Ok(c) => c,
        Err(e) if anyhow_has_status(&e, 404) => {
            // MDH not enabled on this cluster — quietly skip, matching the
            // pull-time tolerance for 403 on permission-gated kinds.
            return Ok((0, 0));
        }
        Err(e) => return Err(e.context("listing MDH collections")),
    };

    progress.set_total(collections.len() as u64);

    let mut used: HashSet<String> = HashSet::new();
    let mut conflicts = 0usize;

    if !collections.is_empty() {
        std::fs::create_dir_all(ctx.paths.mdh_dir())
            .with_context(|| format!("creating {}", ctx.paths.mdh_dir().display()))?;
    }

    // === Phase 1: assign slugs + write collection.json. Build per-collection
    //            dataset_dir map for use in Phase 3.
    let mut dataset_dirs: Vec<(String, std::path::PathBuf, &crate::model::Collection)> = Vec::new();
    for c in &collections {
        let slug = slugify_unique(&c.name, &used);
        used.insert(slug.clone());

        let dataset_dir = ctx.paths.dataset_dir(&slug);
        std::fs::create_dir_all(&dataset_dir)
            .with_context(|| format!("creating {}", dataset_dir.display()))?;

        let coll_path = dataset_dir.join("collection.json");
        let mut coll_proposed = serde_json::to_vec_pretty(c).context("serializing collection")?;
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
        let c_recorded = apply_pull_action(c_action, &coll_path, &coll_proposed, c_remote_hash, ctx.interactive, progress)?;
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

    // === Phase 2: concurrent index fetches per collection (regular +
    //            search). Bounded by ctx.concurrency.
    let client_ref = &client;
    let fetched: Vec<(String, IndexSet)> = futures::stream::iter(
        dataset_dirs.iter().map(|(slug, _, c)| (slug.clone(), c.name.clone()))
    )
    .map(|(slug, name)| async move {
        let regular = client_ref.list_indexes(&name, None).await
            .with_context(|| format!("listing indexes for '{name}'"))?;
        let search = client_ref.list_search_indexes(&name, None).await
            .with_context(|| format!("listing search indexes for '{name}'"))?;
        Ok::<_, anyhow::Error>((slug, IndexSet { regular, search }))
    })
    .buffer_unordered(ctx.concurrency)
    .try_collect()
    .await?;
    let by_slug: std::collections::HashMap<String, IndexSet> = fetched.into_iter().collect();

    // === Phase 3: per-collection indexes.json write decision (sequential
    //            because we mutate ctx.lockfile + counts).
    for (slug, dataset_dir, _) in &dataset_dirs {
        let Some(index_set) = by_slug.get(slug) else { continue };

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
        let i_recorded = apply_pull_action(i_action, &ix_path, &ix_proposed, i_remote_hash, ctx.interactive, progress)?;
        record_object(
            ctx.lockfile,
            "mdh_indexes",
            slug,
            0,
            None,
            None,
            Some(i_recorded),
        );
        progress.tick();
    }

    Ok((collections.len(), conflicts))
}
