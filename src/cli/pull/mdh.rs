use super::common::{hash_for_lockfile, record_object, PullCtx};
use crate::api::DataStorageClient;
use crate::config::EnvConfig;
use crate::model::IndexSet;
use crate::slug::slugify_unique;
use crate::snapshot::collection::write_collection;
use crate::snapshot::index_set::write_index_set;
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Pull all MDH datasets (collection metadata + indexes). No-op when
/// `data_storage_base` is not configured. Returns the count.
pub async fn pull(ctx: &mut PullCtx<'_>, env_cfg: &EnvConfig, token: &str) -> Result<usize> {
    let Some(base) = &env_cfg.data_storage_base else {
        return Ok(0);
    };

    let client = DataStorageClient::new(base.clone(), token.to_string())
        .context("constructing Data Storage client")?;

    let collections = client.list_collections().await.context("listing MDH collections")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    for c in &collections {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.mdh_dir())
                .with_context(|| format!("creating {}", ctx.paths.mdh_dir().display()))?;
            dir_created = true;
        }
        let slug = slugify_unique(&c.name, &used);
        used.insert(slug.clone());

        let dataset_dir = ctx.paths.dataset_dir(&slug);
        std::fs::create_dir_all(&dataset_dir)
            .with_context(|| format!("creating {}", dataset_dir.display()))?;

        // Write collection metadata.
        let coll_bytes = write_collection(&dataset_dir, c)
            .with_context(|| format!("writing collection '{}'", c.name))?;
        let coll_hash = hash_for_lockfile(&coll_bytes);
        record_object(
            ctx.lockfile,
            "mdh_collections",
            &slug,
            // MDH collections have no numeric ID; we use 0 as placeholder.
            0,
            None,
            None,
            Some(coll_hash),
        );

        // Fetch and write indexes.
        let regular = client.list_indexes(&c.name).await
            .with_context(|| format!("listing indexes for '{}'", c.name))?;
        let search = client.list_search_indexes(&c.name).await
            .with_context(|| format!("listing search indexes for '{}'", c.name))?;
        let index_set = IndexSet { regular, search };
        let ix_bytes = write_index_set(&dataset_dir, &index_set)
            .with_context(|| format!("writing indexes for '{}'", c.name))?;
        let ix_hash = hash_for_lockfile(&ix_bytes);
        record_object(
            ctx.lockfile,
            "mdh_indexes",
            &slug,
            0,
            None,
            None,
            Some(ix_hash),
        );
    }

    Ok(collections.len())
}
