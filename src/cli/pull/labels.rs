use super::common::{
    apply_pull_action, decide_pull_action, maybe_strip_overlay, record_object,
    skip_on_permission_denied, PullAction, PullCtx,
};
use crate::model::Label;
use crate::progress::{ResourceOp, ResourceOutcome, SyncRenderer};
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

/// Phase 1: list all labels from the API.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<dyn SyncRenderer>) -> Result<Vec<Label>> {
    skip_on_permission_denied(
        ctx.client.list_labels(Some(progress.clone())).await.context("listing labels"),
        "labels",
        progress,
    )
}

/// Phase 2: write listed labels to disk. `subset` selects which `(kind, slug)`
/// pairs are actually written — items outside the subset are skipped silently
/// (no line, no lockfile update). The sync dispatcher passes a subset filtered
/// by classification (only RemoteEdit/RemoteCreate items). Returns
/// `(count, conflicts)` of items written.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    labels: Vec<Label>,
    subset: &BTreeSet<(String, String)>,
    progress: &Arc<dyn SyncRenderer>,
) -> Result<(usize, usize)> {
    progress.phase("pulling labels");

    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;
    let mut written = 0usize;
    for l in &labels {
        let slug = match ctx.lockfile.slug_for_id("labels", l.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&l.name, &used),
        };
        used.insert(slug.clone());

        if !subset.contains(&("labels".to_string(), slug.clone())) {
            continue;
        }

        progress.resource_started("labels", &slug, ResourceOp::Get);
        let result: Result<()> = (|| {

        if !dir_created {
            std::fs::create_dir_all(ctx.paths.labels_dir())
                .with_context(|| format!("creating {}", ctx.paths.labels_dir().display()))?;
            dir_created = true;
        }

        let mut proposed = serde_json::to_vec_pretty(l).context("serializing label")?;
        proposed.push(b'\n');
        let proposed = maybe_strip_overlay(
            proposed,
            ctx.overlay.as_ref().and_then(|o| o.label(&slug)),
        )?;

        let local_path = ctx.paths.labels_dir().join(format!("{slug}.json"));
        let base_hash = ctx
            .lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get(&slug))
            .and_then(|e| e.content_hash.clone());

        let (action, remote_hash) =
            decide_pull_action(&local_path, base_hash.as_deref(), &proposed)?;
        if action == PullAction::Conflict {
            conflicts += 1;
        }
        let recorded_hash = apply_pull_action(action, &local_path, &proposed, remote_hash, ctx.interactive, progress, ctx.paths.env(), base_hash.as_deref())?;

        record_object(
            ctx.lockfile,
            "labels",
            &slug,
            l.id,
            Some(l.url.clone()),
            l.modified_at().map(|s| s.to_string()),
            Some(recorded_hash),
        );
        written += 1;
        Ok(())
        })();
        let outcome = match &result {
            Ok(()) => ResourceOutcome::Ok,
            Err(e) => ResourceOutcome::Failed(e.to_string()),
        };
        progress.resource_finished("labels", &slug, outcome);
        result?;
    }

    if written > 0 {
        progress.warn_line(&format!("[ok] labels {written} pulled"));
    }

    Ok((written, conflicts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::RossumClient;
    use crate::paths::Paths;
    use crate::state::Lockfile;
    use serde_json::Value;
    use std::collections::BTreeMap;

    fn mk_label(id: u64, name: &str) -> Label {
        Label {
            id,
            url: format!("https://example.invalid/api/v1/labels/{id}"),
            name: name.to_string(),
            organization: "https://example.invalid/api/v1/organizations/1".to_string(),
            extra: BTreeMap::<String, Value>::new(),
        }
    }

    /// `labels::process` must only write items whose slug is in `subset`.
    /// Build two listed labels, include one slug in the subset, and assert
    /// only that one's `.json` lands on disk.
    #[tokio::test]
    async fn labels_process_skips_outside_subset() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        // RossumClient is constructed but never called — labels::process
        // operates on the already-listed Vec<Label>.
        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let mut lockfile = Lockfile::default();
        let progress: Arc<dyn SyncRenderer> = crate::progress::ProgressLog::start("test");

        let mut ctx = PullCtx {
            paths: &paths,
            client: &client,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            overlay: None,
            interactive: false,
        };

        let labels = vec![mk_label(1, "in-scope"), mk_label(2, "out-of-scope")];
        let mut subset = BTreeSet::new();
        subset.insert(("labels".to_string(), "in-scope".to_string()));

        let (written, conflicts) = process(&mut ctx, labels, &subset, &progress).await.unwrap();
        progress.finish_ok("test");

        assert_eq!(written, 1, "only the in-subset label should be written");
        assert_eq!(conflicts, 0);
        assert!(paths.labels_dir().join("in-scope.json").exists(),
            "in-scope label file must exist");
        assert!(!paths.labels_dir().join("out-of-scope.json").exists(),
            "out-of-scope label file must NOT exist");
    }
}
