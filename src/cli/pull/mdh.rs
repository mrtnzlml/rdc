use super::common::{apply_pull_action, decide_pull_action, record_object, PullAction, PullCtx};
use crate::api::DataStorageClient;
use crate::config::EnvConfig;
use crate::model::IndexSet;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::HashSet;

pub async fn pull(ctx: &mut PullCtx<'_>, env_cfg: &EnvConfig, token: &str) -> Result<(usize, usize)> {
    let Some(base) = &env_cfg.data_storage_base else {
        return Ok((0, 0));
    };

    let client = DataStorageClient::new(base.clone(), token.to_string())
        .context("constructing Data Storage client")?;

    let collections = client.list_collections().await.context("listing MDH collections")?;

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;

    for c in &collections {
        if !dir_created {
            std::fs::create_dir_all(ctx.paths.mdh_dir())
                .with_context(|| format!("creating {}", ctx.paths.mdh_dir().display()))?;
            dir_created = true;
        }

        // MDH collections have no numeric ID, so slug stability uses name-based slugify.
        let slug = slugify_unique(&c.name, &used);
        used.insert(slug.clone());

        let dataset_dir = ctx.paths.dataset_dir(&slug);
        std::fs::create_dir_all(&dataset_dir)
            .with_context(|| format!("creating {}", dataset_dir.display()))?;

        // 1. collection.json — three-way
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
        let c_recorded = apply_pull_action(c_action, &coll_path, &coll_proposed, c_remote_hash)?;
        record_object(
            ctx.lockfile,
            "mdh_collections",
            &slug,
            0,
            None,
            None,
            Some(c_recorded),
        );

        // 2. indexes.json — three-way
        let regular = client.list_indexes(&c.name).await
            .with_context(|| format!("listing indexes for '{}'", c.name))?;
        let search = client.list_search_indexes(&c.name).await
            .with_context(|| format!("listing search indexes for '{}'", c.name))?;
        let index_set = IndexSet { regular, search };

        let ix_path = dataset_dir.join("indexes.json");
        let mut ix_proposed = serde_json::to_vec_pretty(&index_set).context("serializing index set")?;
        ix_proposed.push(b'\n');
        let ix_base = ctx
            .lockfile
            .objects
            .get("mdh_indexes")
            .and_then(|m| m.get(&slug))
            .and_then(|e| e.content_hash.clone());
        let (i_action, i_remote_hash) =
            decide_pull_action(&ix_path, ix_base.as_deref(), &ix_proposed)?;
        if i_action == PullAction::Conflict {
            conflicts += 1;
        }
        let i_recorded = apply_pull_action(i_action, &ix_path, &ix_proposed, i_remote_hash)?;
        record_object(
            ctx.lockfile,
            "mdh_indexes",
            &slug,
            0,
            None,
            None,
            Some(i_recorded),
        );
    }

    Ok((collections.len(), conflicts))
}
