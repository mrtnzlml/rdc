use super::common::{
    PullAction, PullCtx, apply_pull_action, decide_pull_action, record_object,
    skip_on_permission_denied,
};
use crate::log::{Action, Log};
use crate::model::SavedView;
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

const KIND: &str = "saved_views";

/// Keep only the views rdc manages.
///
/// Split out so it can be unit-tested without a client.
pub(crate) fn retain_shared(views: Vec<SavedView>) -> Vec<SavedView> {
    views.into_iter().filter(|v| v.shared).collect()
}

/// Phase 1: list saved views, keeping only the SHARED ones.
///
/// The filter MUST happen client-side: the server accepts `?shared=true` and
/// then ignores it, returning private views too (verified on the wire). It is
/// also the safety boundary for the whole kind, not a convenience — a private
/// view belongs to one user, its `query` holds that user's own filter values
/// (customer business data), and `created_by` is read-only so rdc could never
/// restore one to its owner. See the design doc, section B.
pub async fn list(ctx: &PullCtx<'_>, progress: &Arc<Log>) -> Result<Vec<SavedView>> {
    let all = skip_on_permission_denied(
        ctx.client
            .list_saved_views(Some(progress.clone()))
            .await
            .context("listing saved views"),
        KIND,
        progress,
    )?;
    let total = all.len();
    let shared = retain_shared(all);
    let dropped = total - shared.len();
    if dropped > 0 {
        progress.event(
            Action::Skip,
            &format!("saved_views ({dropped} private — rdc manages shared views only)"),
        );
    }
    Ok(shared)
}

/// Phase 2: write listed saved views to disk. `subset` selects which
/// `(kind, slug)` pairs are actually written. Returns `(count, conflicts)`.
pub async fn process(
    ctx: &mut PullCtx<'_>,
    views: Vec<SavedView>,
    subset: &BTreeSet<(String, String)>,
    progress: &Arc<Log>,
) -> Result<(usize, usize)> {
    let mut used: HashSet<String> = HashSet::new();
    let mut dir_created = false;
    let mut conflicts = 0usize;
    let mut written = 0usize;
    for v in &views {
        let slug = match ctx.lockfile.slug_for_id(KIND, v.id) {
            Some(existing) => existing.to_string(),
            None => slugify_unique(&v.name, &used),
        };
        used.insert(slug.clone());

        if !subset.contains(&(KIND.to_string(), slug.clone())) {
            continue;
        }

        let result: Result<()> = (|| {
            if !dir_created {
                std::fs::create_dir_all(ctx.paths.saved_views_dir()).with_context(|| {
                    format!("creating {}", ctx.paths.saved_views_dir().display())
                })?;
                dir_created = true;
            }

            let value = serde_json::to_value(v)?;
            let art = crate::snapshot::codec::codec(KIND)
                .unwrap()
                .disk_bytes(&value)
                .context("serializing saved view")?;
            let proposed = art.json;

            let local_path = ctx.paths.saved_views_dir().join(format!("{slug}.json"));
            let base_hash = ctx
                .lockfile
                .objects
                .get(KIND)
                .and_then(|m| m.get(&slug))
                .and_then(|e| e.content_hash.clone());

            let proposed =
                crate::cli::pull::common::portabilize_proposed(&proposed, &*ctx.lockfile);
            let (action, remote_hash) =
                decide_pull_action(&local_path, base_hash.as_deref(), &proposed)?;
            if action == PullAction::Conflict {
                conflicts += 1;
            }
            let recorded_hash = apply_pull_action(
                action,
                &local_path,
                &proposed,
                remote_hash,
                ctx.interactive,
                progress,
                ctx.paths.env(),
                base_hash.as_deref(),
                Some(ctx.paths),
            )?;

            record_object(
                ctx.lockfile,
                KIND,
                &slug,
                v.id,
                v.modified_at().map(|s| s.to_string()),
                v.modified_by().map(|s| s.to_string()),
                Some(recorded_hash),
            );
            written += 1;
            Ok(())
        })();
        result?;
    }

    if written > 0 {
        progress.event(Action::Pull, &format!("saved_views ({written} pulled)"));
    }

    Ok((written, conflicts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::RossumClient;
    use crate::paths::Paths;
    use crate::state::Lockfile;
    use serde_json::json;

    fn mk(id: u64, name: &str, shared: bool) -> SavedView {
        SavedView {
            id,
            url: format!("https://example.invalid/api/v1/saved_views/{id}"),
            name: name.to_string(),
            shared,
            queues_filter: Vec::new(),
            query: json!({ "$and": [] }),
            extra: indexmap::IndexMap::new(),
        }
    }

    #[tokio::test]
    async fn process_writes_only_subset_members() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let mut lockfile = Lockfile::default();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);

        let mut ctx = PullCtx {
            paths: &paths,
            client: &client,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            interactive: false,
        };

        let views = vec![mk(1, "in scope", true), mk(2, "out of scope", true)];
        let mut subset = BTreeSet::new();
        subset.insert(("saved_views".to_string(), "in-scope".to_string()));

        let (written, conflicts) = process(&mut ctx, views, &subset, &progress).await.unwrap();

        assert_eq!(written, 1);
        assert_eq!(conflicts, 0);
        assert!(paths.saved_views_dir().join("in-scope.json").exists());
        assert!(!paths.saved_views_dir().join("out-of-scope.json").exists());
    }

    /// Two views with the same name are routine: the API does not enforce
    /// uniqueness, and per-user namespaces make collisions common.
    #[tokio::test]
    async fn duplicate_names_get_suffixed_slugs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let mut lockfile = Lockfile::default();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let mut ctx = PullCtx {
            paths: &paths,
            client: &client,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            interactive: false,
        };

        let views = vec![mk(1, "Shared filter", true), mk(2, "Shared filter", true)];
        let mut subset = BTreeSet::new();
        subset.insert(("saved_views".to_string(), "shared-filter".to_string()));
        subset.insert(("saved_views".to_string(), "shared-filter-2".to_string()));

        let (written, _) = process(&mut ctx, views, &subset, &progress).await.unwrap();
        assert_eq!(written, 2);
        assert!(paths.saved_views_dir().join("shared-filter.json").exists());
        assert!(paths.saved_views_dir().join("shared-filter-2.json").exists());
    }

    /// The entire cross-env story for this kind rests on the generic walker
    /// reaching refs nested inside `query`. Pin it rather than trusting that it
    /// "comes for free": portabilize must rewrite a queue URL at depth, resolve
    /// must restore it, and a `field.<schema_id>` KEY must be left alone
    /// because `walk_strings_mut` never visits object keys.
    #[test]
    fn nested_query_queue_ref_round_trips_through_portabilize() {
        use crate::snapshot::refs::{portabilize_value, resolve_value};
        let mut lockfile = Lockfile::default();
        lockfile.api_base = "https://acme.rossum.app/api/v1".to_string();
        lockfile.upsert(
            "queues",
            "invoices",
            crate::state::ObjectEntry {
                id: 100,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );

        let url = "https://acme.rossum.app/api/v1/queues/100";
        let mut v = json!({
            "queues_filter": [url],
            "query": { "$and": [
                { "queue": { "$in": [url] } },
                { "field.document_id.string": { "$eq": "x" } }
            ] }
        });

        portabilize_value(&mut v, &lockfile);
        assert_eq!(v["queues_filter"][0], json!("rdc://queues/invoices"));
        assert_eq!(
            v["query"]["$and"][0]["queue"]["$in"][0],
            json!("rdc://queues/invoices"),
            "a ref nested three levels into query must portabilize"
        );
        assert!(
            v["query"]["$and"][1].get("field.document_id.string").is_some(),
            "a schema-field id is an object KEY and must be untouched by design"
        );

        resolve_value(&mut v, &lockfile);
        assert_eq!(v["queues_filter"][0], json!(url));
        assert_eq!(v["query"]["$and"][0]["queue"]["$in"][0], json!(url));
    }

    #[test]
    fn filter_keeps_only_shared() {
        let all = vec![mk(1, "public", true), mk(2, "mine", false), mk(3, "also public", true)];
        let kept = retain_shared(all);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|v| v.shared));
    }

    /// This is the safety boundary's actual enforcement site, not just the
    /// pure `retain_shared` helper: a real listing response containing both
    /// a shared and a private view must come back through `list()` with the
    /// private one already gone. `filter_keeps_only_shared` above tests the
    /// helper in isolation and would keep passing even if `list()` stopped
    /// calling it — this test would not.
    #[tokio::test]
    async fn list_filters_out_private_views_over_the_wire() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = serde_json::json!({
            "pagination": { "next": null },
            "results": [
                {
                    "id": 1,
                    "url": format!("{}/api/v1/saved_views/1", server.uri()),
                    "name": "Team dashboard",
                    "shared": true,
                    "queues_filter": [],
                    "query": { "$and": [] }
                },
                {
                    "id": 2,
                    "url": format!("{}/api/v1/saved_views/2", server.uri()),
                    "name": "My private filter",
                    "shared": false,
                    "queues_filter": [],
                    "query": { "$and": [] }
                }
            ]
        });
        Mock::given(method("GET"))
            .and(path("/api/v1/saved_views"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let client =
            RossumClient::new(format!("{}/api/v1", server.uri()), "TEST".to_string()).unwrap();
        let mut lockfile = Lockfile::default();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let ctx = PullCtx {
            paths: &paths,
            client: &client,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            interactive: false,
        };

        let views = list(&ctx, &progress).await.unwrap();

        assert_eq!(views.len(), 1, "the private view must not survive list()");
        assert_eq!(views[0].id, 1);
        assert_eq!(views[0].name, "Team dashboard");
    }
}
