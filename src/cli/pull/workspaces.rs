use super::common::{hash_for_lockfile, record_object, skip_on_permission_denied, PullCtx};
use crate::config::EnvConfig;
use crate::model::Workspace;
use crate::progress::OverallProgress;
use crate::slug::slugify_unique;
use crate::snapshot::workspace::write_workspace;
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashSet;
use std::sync::Arc;

/// Phase 1: list all workspaces from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<OverallProgress>) -> Result<Vec<Workspace>> {
    skip_on_permission_denied(
        ctx.client.list_workspaces(Some(progress.clone())).await.context("listing workspaces"),
        "workspaces",
        progress,
    )
}

/// Phase 2: filter and write listed workspaces to disk.
/// When the env's `workspace_filter` is set, only matching workspaces are
/// written; all others are ticked (counted) but not written.
/// Returns the number of workspaces written.
pub async fn process(ctx: &mut PullCtx<'_>, workspaces: Vec<Workspace>, env_cfg: &EnvConfig, progress: &Arc<OverallProgress>) -> Result<usize> {
    progress.start_phase("workspaces");

    let filter = match &env_cfg.workspace_filter {
        Some(pat) => Some(
            Regex::new(pat)
                .with_context(|| format!("compiling workspace_filter regex '{pat}'"))?,
        ),
        None => None,
    };

    let mut used_slugs: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut count = 0usize;
    for ws in &workspaces {
        if let Some(re) = &filter {
            if !re.is_match(&ws.name) {
                progress.tick(&ws.name);
                continue;
            }
        }

        if !dir_created {
            std::fs::create_dir_all(ctx.paths.workspaces_dir())
                .with_context(|| format!("creating {}", ctx.paths.workspaces_dir().display()))?;
            dir_created = true;
        }

        let slug = match ctx.lockfile.slug_for_id("workspaces", ws.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&ws.name, &used_slugs),
        };
        used_slugs.insert(slug.clone());

        let ws_dir = ctx.paths.workspace_dir(&slug);
        std::fs::create_dir_all(&ws_dir)
            .with_context(|| format!("creating {}", ws_dir.display()))?;

        let bytes = write_workspace(&ws_dir, ws)
            .with_context(|| format!("writing workspace '{}' to disk", ws.name))?;
        let hash = hash_for_lockfile(&bytes);

        record_object(
            ctx.lockfile,
            "workspaces",
            &slug,
            ws.id,
            Some(ws.url.clone()),
            ws.modified_at().map(|s| s.to_string()),
            Some(hash),
        );

        count += 1;
        progress.tick(&ws.name);
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    #[test]
    fn filter_regex_matches_dev_prefix() {
        let re = Regex::new("^DEV ").unwrap();
        assert!(re.is_match("DEV Workspace"));
        assert!(!re.is_match("PROD Workspace"));
        assert!(!re.is_match("My DEV Workspace"));
    }

    #[test]
    fn filter_regex_with_alternation() {
        let re = Regex::new("(?i)(invoices|orders)").unwrap();
        assert!(re.is_match("Invoices AP"));
        assert!(re.is_match("Purchase Orders"));
        assert!(!re.is_match("HR"));
    }
}
