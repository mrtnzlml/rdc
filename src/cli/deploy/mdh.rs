//! Cross-env deploy of MDH (Master Data Hub) dataset indexes.
//!
//! `rdc deploy` propagates the source env's per-dataset `indexes.json` to the
//! target's matching collection: regular + Atlas Search index definitions are
//! reconciled (create missing, recreate changed; prune target-only extras only
//! under `--mirror`). The dataset DATA (rows) is never touched — only index
//! definitions — and the target collection must already exist (rdc has no
//! collection-create API; importing a dataset's data is a UI-side concern).
//!
//! Datasets are matched by slug (= slugified collection name, assigned the same
//! way the pull driver does); a source dataset with no matching target
//! collection is warned + skipped, and MDH not being enabled on the target
//! (404) skips MDH entirely — both matching the read-side behavior.
//!
//! The reconcile itself (diff + drop/create with same-name drop-wait) is shared
//! with the within-env push driver via [`crate::cli::push::mdh`].

use crate::api::{anyhow_has_status, DataStorageClient};
use crate::cli::push::mdh::{apply_diff, diff_indexes};
use crate::log::{Action, Log};
use crate::model::IndexSet;
use crate::paths::Paths;
use crate::slug::slugify_unique;
use anyhow::{anyhow, Context, Result};
use std::collections::HashSet;
use std::sync::Arc;

/// What a deploy MDH pass did (or, in dry-run, would do).
#[derive(Debug, Default, Clone, Copy)]
pub struct MdhApplied {
    /// Datasets with at least one index op.
    pub datasets: usize,
    /// Total index write ops (drops + creates).
    pub ops: usize,
    /// Source datasets skipped because the target collection is absent.
    pub skipped: usize,
}

/// Deploy the source env's MDH index definitions to the target.
///
/// `mirror` gates pruning of target-only indexes (see [`diff_indexes`]).
/// `dry_run` computes + reports the plan without issuing any writes. `progress`
/// is `Some` only for the real apply (where per-index events are emitted); the
/// dry-run path passes `None` and the caller reports the returned counts.
pub async fn deploy_mdh_indexes(
    src_paths: &Paths,
    tgt_client: &DataStorageClient,
    mirror: bool,
    dry_run: bool,
    progress: Option<Arc<Log>>,
) -> Result<MdhApplied> {
    let mut applied = MdhApplied::default();

    let src_datasets = list_local_dataset_slugs(src_paths)?;
    if src_datasets.is_empty() {
        return Ok(applied);
    }

    // Target collections — skip MDH entirely if not enabled (404), matching
    // the pull driver's quiet-skip.
    let collections = match tgt_client.list_collections(progress.clone()).await {
        Ok(c) => c,
        Err(e) if anyhow_has_status(&e, 404) => {
            if let Some(p) = &progress {
                p.event(Action::Skip, "mdh (Master Data Hub not enabled on target)");
            }
            return Ok(applied);
        }
        Err(e) => return Err(e.context("listing target MDH collections")),
    };
    // slug -> collection name, slugged the same way the pull driver assigns
    // dataset dirs, so a same-named dataset lines up across envs.
    let mut used: HashSet<String> = HashSet::new();
    let mut tgt_by_slug: std::collections::HashMap<String, String> = Default::default();
    for c in &collections {
        let slug = slugify_unique(&c.name, &used);
        used.insert(slug.clone());
        tgt_by_slug.insert(slug, c.name.clone());
    }

    for slug in &src_datasets {
        let Some(tgt_name) = tgt_by_slug.get(slug) else {
            if let Some(p) = &progress {
                p.event(
                    Action::Warn,
                    &format!(
                        "mdh/{slug}: no matching dataset on target; skipping \
                         (import its data on the target first)"
                    ),
                );
            }
            applied.skipped += 1;
            continue;
        };

        let indexes_path = src_paths.dataset_dir(slug).join("indexes.json");
        let local_raw = std::fs::read(&indexes_path)
            .with_context(|| format!("reading {}", indexes_path.display()))?;
        let local_set: IndexSet = serde_json::from_slice(&local_raw)
            .with_context(|| format!("parsing {}", indexes_path.display()))?;

        // Diff against the target's LIVE indexes (not a lockfile baseline):
        // an admin may have changed them out-of-band, and deploy reconciles
        // toward the source's desired state.
        let remote_regular = tgt_client
            .list_indexes(tgt_name, progress.clone())
            .await
            .with_context(|| format!("listing regular indexes for target '{tgt_name}'"))?;
        let remote_search = tgt_client
            .list_search_indexes(tgt_name, progress.clone())
            .await
            .with_context(|| format!("listing search indexes for target '{tgt_name}'"))?;

        let plan = diff_indexes(
            &local_set.regular,
            &local_set.search,
            &remote_regular,
            &remote_search,
            mirror,
        );
        let plan_ops = plan.drop_regular.len()
            + plan.drop_search.len()
            + plan.create_regular.len()
            + plan.create_search.len();
        if plan_ops == 0 {
            continue; // target already matches the source's index set
        }
        applied.datasets += 1;

        if dry_run {
            if let Some(p) = &progress {
                for n in &plan.drop_regular {
                    p.event(Action::Warn, &format!("mdh/{slug} regular index '{n}' (would drop)"));
                }
                for n in &plan.drop_search {
                    p.event(Action::Warn, &format!("mdh/{slug} search index '{n}' (would drop)"));
                }
                for def in &plan.create_regular {
                    let n = def.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    p.event(Action::Info, &format!("mdh/{slug} regular index '{n}' (would create)"));
                }
                for def in &plan.create_search {
                    let n = def.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    p.event(Action::Info, &format!("mdh/{slug} search index '{n}' (would create)"));
                }
            }
            applied.ops += plan_ops;
        } else {
            let log = progress
                .as_ref()
                .ok_or_else(|| anyhow!("internal: MDH deploy apply requires a progress handle"))?;
            applied.ops += apply_diff(tgt_client, tgt_name, slug, &plan, log).await?;
        }
    }

    Ok(applied)
}

/// Source datasets are the `mdh/<slug>/` dirs that carry an `indexes.json`.
fn list_local_dataset_slugs(paths: &Paths) -> Result<Vec<String>> {
    let dir = paths.mdh_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let slug = entry.file_name().to_string_lossy().to_string();
        if paths.dataset_dir(&slug).join("indexes.json").exists() {
            out.push(slug);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_local_dataset_slugs_finds_only_dirs_with_indexes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "src");
        // Two datasets with indexes.json + one dir without (partial state).
        for slug in ["vendors", "products"] {
            let d = paths.dataset_dir(slug);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("indexes.json"), b"{\"regular\":[],\"search\":[]}").unwrap();
        }
        std::fs::create_dir_all(paths.dataset_dir("partial")).unwrap();

        let got = list_local_dataset_slugs(&paths).unwrap();
        assert_eq!(got, vec!["products".to_string(), "vendors".to_string()]);
    }

    #[test]
    fn list_local_dataset_slugs_empty_when_no_mdh_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "src");
        assert!(list_local_dataset_slugs(&paths).unwrap().is_empty());
    }
}
