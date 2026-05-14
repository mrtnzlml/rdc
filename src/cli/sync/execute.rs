//! Execute the classified plan. Dispatches three branches:
//!
//! - **Conflict (`BothDiverged`)** — runs first via [`resolve_conflicts`].
//!   Each conflict prompts the user (`[k]/[r]/[e]/[s]/[a]`), then routes the
//!   outcome: keep-local / edit promote the item to LocalEdit (pushed
//!   below); keep-remote writes the remote bytes to disk + lockfile;
//!   skip writes a shadow file and records the local hash; abort bubbles
//!   [`crate::cli::resolve::PullAborted`].
//! - **Pull-side (`RemoteEdit`, `RemoteCreate`)** — grouped by kind and
//!   handed off to the per-kind pull driver with a `(kind, slug)` subset
//!   filter.
//! - **Push-side (`LocalEdit`, `LocalCreate`)** — folded into a
//!   `ChangeList` via [`crate::cli::push::scan::change_list_from_classified`]
//!   and dispatched through the existing push pipeline. Items promoted
//!   from the conflict branch (KeepLocal / Edit) are merged into the same
//!   ChangeList so they take a single round-trip through the push driver.
//!
//! Remote-delete and the two double-conflict classes land in Task 17.
//!
//! Spec: docs/superpowers/specs/2026-05-14-unified-sync-design.md.

use crate::cli::pull::common::{PullCtx, RemoteCatalog};
use crate::cli::resolve::{prompt_resolve, PullAborted, Resolution};
use crate::cli::sync::classify::{ClassifiedItem, SyncClass};
use crate::progress::OverallProgress;
use crate::slug::slugify_unique;
use crate::snapshot::writer::write_atomic;
use crate::state::content_hash;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;

/// Items the conflict resolver promoted into the push pipeline. The caller
/// merges these into the push-side `ChangeList` so a single PATCH round
/// covers both the original `LocalEdit`/`LocalCreate` items and the
/// `KeepLocal`/`Edit` outcomes of `BothDiverged` items.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ConflictOutcome {
    /// `(kind, slug, on-disk path)` triples to feed into the push driver.
    pub(crate) promoted_to_push: Vec<(String, String, PathBuf)>,
}

/// Resolve each `BothDiverged` item in `classified`. The resolver prompt
/// reads from `input` (production passes a locked stdin; tests pass a
/// `Cursor`). On `[k]`/`[e]` the item is promoted to the push side (the
/// caller PATCHes it); on `[r]` the file is overwritten in place and the
/// lockfile records the remote hash; on `[s]` the legacy shadow-file is
/// written; on `[a]` the call returns a [`PullAborted`]-wrapping error.
///
/// `interactive == false` is a no-op for conflicts in unified sync —
/// `apply_pull_action`'s legacy shadow-file branch handled the non-TTY
/// case in the pre-Task-16 pipeline, but the new dispatch requires an
/// explicit user decision (the spec calls this "rejected without
/// prompt" semantically). The function still emits a shadow file +
/// warning per item so CI runs aren't surprising.
///
/// Today only `labels` are wired (matching Task 14/15 scope); other
/// kinds fall through with a warning. Add per-kind arms as their
/// adapters arrive.
pub(crate) async fn resolve_conflicts<R: BufRead>(
    ctx: &mut PullCtx<'_>,
    catalog: &RemoteCatalog,
    classified: &[ClassifiedItem],
    mut input: R,
    interactive: bool,
    progress: &Arc<OverallProgress>,
) -> Result<ConflictOutcome> {
    let mut outcome = ConflictOutcome::default();

    // Build a stable label-slug index up-front. This mirrors the slug
    // derivation rule used by `pull::labels::process` and
    // `from_catalog_scan_lockfile` so the slug we look up here matches
    // the slug the classifier emitted.
    let mut label_by_slug: BTreeMap<String, &crate::model::Label> = BTreeMap::new();
    {
        let mut used: HashSet<String> = HashSet::new();
        for l in &catalog.labels {
            let slug = match ctx.lockfile.slug_for_id("labels", l.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&l.name, &used),
            };
            used.insert(slug.clone());
            label_by_slug.insert(slug, l);
        }
    }

    let conflicts: Vec<&ClassifiedItem> = classified
        .iter()
        .filter(|it| it.class == SyncClass::BothDiverged)
        .collect();
    let total = conflicts.len();
    if total == 0 {
        return Ok(outcome);
    }

    let env = ctx.paths.env().to_string();
    let stderr = std::io::stderr();
    let mut stderr_lock = stderr.lock();

    for (idx, it) in conflicts.iter().enumerate() {
        match it.kind.as_str() {
            "labels" => {
                let Some(label) = label_by_slug.get(it.slug.as_str()) else {
                    // The classifier said BothDiverged but the catalog
                    // doesn't carry this slug. Defensive skip — surfaces as
                    // a warning rather than a panic so the rest of the sync
                    // can still proceed.
                    progress.println(format!(
                        "warning: conflict for labels/{} but no matching remote label found; skipping",
                        it.slug
                    ));
                    continue;
                };

                let mut remote_bytes = serde_json::to_vec_pretty(label)
                    .context("serializing remote label for conflict resolver")?;
                remote_bytes.push(b'\n');

                let local_path = ctx.paths.labels_dir().join(format!("{}.json", it.slug));

                if !interactive {
                    // Non-TTY/--yes: fall back to legacy shadow-file
                    // behavior so the run still completes without
                    // blocking on stdin. The local file stays as-is and
                    // the lockfile records the local hash.
                    let conflict_path = crate::paths::shadow_path_for(&local_path, &env);
                    write_atomic(&conflict_path, &remote_bytes)?;
                    progress.println(format!(
                        "warning: {} conflict — local preserved, remote at {}",
                        local_path.display(),
                        conflict_path.display(),
                    ));
                    let local_bytes = std::fs::read(&local_path)
                        .with_context(|| format!("reading {}", local_path.display()))?;
                    let local_hash = content_hash(&local_bytes);
                    update_label_lockfile(ctx, &it.slug, label, local_hash);
                    continue;
                }

                let resolution = prompt_resolve(
                    &mut input,
                    &mut stderr_lock,
                    idx + 1,
                    total,
                    &local_path,
                    &remote_bytes,
                    &env,
                )?;

                match resolution {
                    Resolution::KeepLocal => {
                        // Local wins → must PATCH. The push driver
                        // re-reads `local_path` and PATCHes the remote;
                        // it also drives its own drift detection which
                        // will now see local-vs-remote drift again, but
                        // the `[k]eep local` decision means the user
                        // accepted force-push.
                        //
                        // Pre-write a lockfile base that matches the
                        // current remote so the push driver's drift
                        // check passes (remote_hash == base). The PATCH
                        // response updates the lockfile to the post-PATCH
                        // canonical form.
                        let remote_hash = content_hash(&remote_bytes);
                        update_label_lockfile(ctx, &it.slug, label, remote_hash);
                        outcome
                            .promoted_to_push
                            .push(("labels".to_string(), it.slug.clone(), local_path));
                    }
                    Resolution::KeepRemote => {
                        write_atomic(&local_path, &remote_bytes)?;
                        let remote_hash = content_hash(&remote_bytes);
                        update_label_lockfile(ctx, &it.slug, label, remote_hash);
                    }
                    Resolution::Edit(edited) => {
                        write_atomic(&local_path, &edited)?;
                        // Same rationale as KeepLocal: align base to
                        // remote so push drift detection succeeds, then
                        // PATCH the edited bytes.
                        let remote_hash = content_hash(&remote_bytes);
                        update_label_lockfile(ctx, &it.slug, label, remote_hash);
                        outcome
                            .promoted_to_push
                            .push(("labels".to_string(), it.slug.clone(), local_path));
                    }
                    Resolution::Skip => {
                        let conflict_path = crate::paths::shadow_path_for(&local_path, &env);
                        write_atomic(&conflict_path, &remote_bytes)?;
                        progress.println(format!(
                            "warning: {} conflict — local preserved, remote at {}",
                            local_path.display(),
                            conflict_path.display(),
                        ));
                        let local_bytes = std::fs::read(&local_path)
                            .with_context(|| format!("reading {}", local_path.display()))?;
                        let local_hash = content_hash(&local_bytes);
                        update_label_lockfile(ctx, &it.slug, label, local_hash);
                    }
                    Resolution::Abort => {
                        return Err(anyhow::Error::new(PullAborted));
                    }
                }
            }
            // TODO(sync-impl): add per-kind arms as their hashing and
            // pull/push adapters arrive. Each kind needs (a) a way to
            // serialize remote bytes from the catalog and (b) a local
            // path, both of which already live in the per-kind pull/push
            // drivers — extract a small helper trait when the second
            // kind lands rather than duplicating this match.
            other => {
                progress.println(format!(
                    "warning: conflict resolver not yet wired for kind '{}' (slug '{}'); skipping",
                    other, it.slug,
                ));
            }
        }
    }

    Ok(outcome)
}

/// Update the lockfile entry for a label so the recorded hash matches
/// `content_hash`. Used by every resolution branch to keep the lockfile
/// consistent with what's on disk (or, for KeepLocal/Edit, with what
/// will be pushed). The push driver expects `base == remote_hash` to
/// pass its drift detection; pre-seeding here is what unlocks the
/// force-push semantics of `[k]eep local`.
fn update_label_lockfile(
    ctx: &mut PullCtx<'_>,
    slug: &str,
    label: &crate::model::Label,
    content_hash: String,
) {
    crate::cli::pull::common::record_object(
        ctx.lockfile,
        "labels",
        slug,
        label.id,
        Some(label.url.clone()),
        label.modified_at().map(|s| s.to_string()),
        Some(content_hash),
    );
}

/// Dispatch the classified items. The order is: resolve conflicts → run
/// pull-side drivers → run push-side pipeline (with conflict-promoted
/// items merged in). Items promoted from the conflict branch land in the
/// same `ChangeList` slot as native `LocalEdit`s, so the push driver
/// can't tell them apart.
///
/// `interactive` flows through to the conflict resolver and the push
/// driver's drift prompt; on the pull side each per-kind driver consults
/// `ctx.interactive` (set by the caller to the same value).
pub async fn run(
    ctx: &mut PullCtx<'_>,
    catalog: &RemoteCatalog,
    classified: &[ClassifiedItem],
    no_push: bool,
    no_pull: bool,
    interactive: bool,
    progress: &Arc<OverallProgress>,
) -> Result<()> {
    // Phase A: conflicts. Runs first so the user resolves drift before
    // the executor commits to either side. The stdin read here is the
    // only blocking-on-user-input call in the executor; the helper takes
    // a generic `BufRead` so tests can drive it with a `Cursor`.
    let stdin = std::io::stdin();
    let conflict_outcome =
        resolve_conflicts(ctx, catalog, classified, stdin.lock(), interactive, progress).await?;

    if !no_pull {
        // Group pull-side items by kind so each driver runs at most
        // once per sync. Slugs inside the subset filter through the
        // driver's own `subset.contains(...)` guard.
        let mut subsets: BTreeMap<&str, BTreeSet<(String, String)>> = BTreeMap::new();
        for it in classified {
            if matches!(it.class, SyncClass::RemoteEdit | SyncClass::RemoteCreate) {
                subsets
                    .entry(it.kind.as_str())
                    .or_default()
                    .insert((it.kind.clone(), it.slug.clone()));
            }
        }

        // labels: flat slug, no nested files. Wired today because Task
        // 14 ships the labels-only adapter; other kinds plug in as
        // their adapter coverage lands.
        if let Some(subset) = subsets.get("labels") {
            crate::cli::pull::labels::process(ctx, catalog.labels.clone(), subset, progress).await?;
        }

        // TODO(sync-impl): add per-kind dispatch as their adapter
        // hashing arrives — workspaces, queues, schemas, inboxes,
        // hooks, rules, engines, engine_fields, email_templates,
        // workflows, workflow_steps, mdh. Each kind's `process` already
        // accepts a subset filter; the only new code is the
        // `subsets.get("<kind>") → process(...)` line plus any
        // catalog-side prerequisites (e.g. queues needs the workspace
        // map populated; see `pull::run_drivers` for ordering).
    }

    if !no_push {
        // Fold LocalEdit / LocalCreate items into the same `ChangeList`
        // shape `push::scan::scan` produces, then merge in the items
        // the conflict resolver promoted (`[k]eep local` / `[e]dit`).
        let mut change_list =
            crate::cli::push::scan::change_list_from_classified(ctx.paths, classified);
        for (kind, slug, path) in conflict_outcome.promoted_to_push {
            match kind.as_str() {
                "labels" => {
                    change_list.labels.insert(slug, path);
                }
                // TODO(sync-impl): mirror the change_list_from_classified
                // kind-table once more kinds enter the conflict path.
                _ => {}
            }
        }
        if !change_list.is_empty() {
            let env = ctx.paths.env().to_string();
            crate::cli::push::push_classified(
                ctx.paths,
                ctx.client,
                ctx.lockfile,
                &env,
                interactive,
                &change_list,
                progress,
            )
            .await?;
        }
    }

    // Remote-delete + double-conflict branches: Task 17.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::RossumClient;
    use crate::cli::pull::common::RemoteCatalog;
    use crate::model::Label;
    use crate::paths::Paths;
    use crate::state::{Lockfile, ObjectEntry};
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::io::Cursor;

    /// Build an empty RemoteCatalog with `labels` populated by the caller.
    /// Other fields default to empty so the helper can construct a minimal
    /// catalog without bringing in every model type.
    fn catalog_with_labels(labels: Vec<Label>) -> RemoteCatalog {
        RemoteCatalog {
            organization: crate::model::Organization {
                id: 1,
                url: "https://x.invalid/api/v1/organizations/1".to_string(),
                name: "test".to_string(),
                extra: BTreeMap::new(),
            },
            workspaces: vec![],
            queues: vec![],
            hooks: vec![],
            rules: vec![],
            labels,
            engines: vec![],
            engine_fields: vec![],
            workflows: vec![],
            workflow_steps: vec![],
            email_templates: vec![],
            mdh: crate::cli::pull::mdh::MdhListed {
                client: crate::api::data_storage::DataStorageClient::new(
                    "https://unused.invalid/svc/data-storage/api/v1".to_string(),
                    "TEST".to_string(),
                )
                .unwrap(),
                collections: vec![],
            },
        }
    }

    fn mk_label(id: u64, name: &str, color: &str) -> Label {
        let mut extra: BTreeMap<String, Value> = BTreeMap::new();
        extra.insert("color".to_string(), Value::String(color.to_string()));
        Label {
            id,
            url: format!("https://x.invalid/api/v1/labels/{id}"),
            name: name.to_string(),
            organization: "https://x.invalid/api/v1/organizations/1".to_string(),
            extra,
        }
    }

    /// Serialize a label exactly the way `pull::labels::process` does so
    /// the local on-disk bytes match what the pull driver would have
    /// written. Used to seed the "initial sync" state in tests.
    fn label_bytes(l: &Label) -> Vec<u8> {
        let mut b = serde_json::to_vec_pretty(l).unwrap();
        b.push(b'\n');
        b
    }

    /// Scaffolding for a one-label conflict scenario: a temp dir + Paths
    /// + Lockfile + a label that exists locally with the BASE bytes and
    /// has a divergent remote variant. The caller supplies the local
    /// edit and the remote variant separately to set up the BothDiverged
    /// state.
    struct ConflictFixture {
        _tmp: tempfile::TempDir,
        paths: Paths,
        client: RossumClient,
        lockfile: Lockfile,
        local_path: PathBuf,
        local_edit: Label,
        remote_label: Label,
    }

    fn setup_conflict_fixture() -> ConflictFixture {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.labels_dir()).unwrap();

        // BASE bytes — what the lockfile records.
        let base = mk_label(99, "Audit Hold", "#aabbcc");
        let base_bytes = label_bytes(&base);
        let base_hash = content_hash(&base_bytes);

        // LOCAL edit — different color from base; lives on disk.
        let local_edit = mk_label(99, "Audit Hold", "#ff0000");
        let local_bytes = label_bytes(&local_edit);
        let local_path = paths.labels_dir().join("audit-hold.json");
        std::fs::write(&local_path, &local_bytes).unwrap();

        // REMOTE — different color from both base and local.
        let remote_label = mk_label(99, "Audit Hold", "#00ff00");

        // Lockfile: records base hash so a re-pull would classify as
        // BothDiverged once local edits land.
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "labels",
            "audit-hold",
            ObjectEntry {
                id: 99,
                url: Some(remote_label.url.clone()),
                modified_at: None,
                content_hash: Some(base_hash),
            },
        );

        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();

        ConflictFixture {
            _tmp: tmp,
            paths,
            client,
            lockfile,
            local_path,
            local_edit,
            remote_label,
        }
    }

    fn classified_for(fixture: &ConflictFixture) -> Vec<ClassifiedItem> {
        let local_hash = content_hash(&label_bytes(&fixture.local_edit));
        let remote_hash = content_hash(&label_bytes(&fixture.remote_label));
        let base_hash = fixture
            .lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get("audit-hold"))
            .and_then(|e| e.content_hash.clone())
            .unwrap();
        vec![ClassifiedItem {
            kind: "labels".to_string(),
            slug: "audit-hold".to_string(),
            class: SyncClass::BothDiverged,
            local_hash: Some(local_hash),
            remote_hash: Some(remote_hash),
            base_hash: Some(base_hash),
        }]
    }

    /// Scripted `k\n` ([k]eep local) on a BothDiverged label: the
    /// resolver must promote the item to the push pipeline (caller
    /// PATCHes), leave the local file unchanged, and align the lockfile
    /// to the remote hash (so the push driver's drift check passes).
    #[tokio::test]
    async fn resolve_conflicts_keep_local_promotes_to_push_and_aligns_lockfile() {
        let mut fixture = setup_conflict_fixture();
        let catalog = catalog_with_labels(vec![fixture.remote_label.clone()]);
        let classified = classified_for(&fixture);
        let progress = OverallProgress::start("test");

        // Snapshot local bytes so we can assert they survive.
        let local_before = std::fs::read(&fixture.local_path).unwrap();

        let outcome = {
            let mut ctx = PullCtx {
                paths: &fixture.paths,
                client: &fixture.client,
                lockfile: &mut fixture.lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            resolve_conflicts(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b"k\n"),
                true,
                &progress,
            )
            .await
            .expect("resolver should succeed on [k]")
        };
        progress.finish();

        // Outcome: one entry promoted to the push pipeline, naming the
        // label slug and its on-disk path.
        assert_eq!(outcome.promoted_to_push.len(), 1, "should promote 1 item");
        assert_eq!(outcome.promoted_to_push[0].0, "labels");
        assert_eq!(outcome.promoted_to_push[0].1, "audit-hold");
        assert_eq!(outcome.promoted_to_push[0].2, fixture.local_path);

        // Local file unchanged — the caller (push driver) will read it
        // when it PATCHes, so it must still hold the user's edit.
        let local_after = std::fs::read(&fixture.local_path).unwrap();
        assert_eq!(local_after, local_before, "local file must survive [k]");

        // Lockfile aligned to remote hash so the push driver's drift
        // check accepts the force-push.
        let remote_bytes = label_bytes(&fixture.remote_label);
        let remote_hash = content_hash(&remote_bytes);
        let recorded = fixture
            .lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get("audit-hold"))
            .and_then(|e| e.content_hash.clone())
            .expect("lockfile entry must persist");
        assert_eq!(recorded, remote_hash, "lockfile base should now equal remote hash");
    }

    /// Scripted `r\n` ([r]use env): the resolver overwrites the local
    /// file with remote bytes, updates the lockfile to the remote hash,
    /// and does NOT promote the item to push (the env wins).
    #[tokio::test]
    async fn resolve_conflicts_use_remote_overwrites_local_and_skips_push() {
        let mut fixture = setup_conflict_fixture();
        let catalog = catalog_with_labels(vec![fixture.remote_label.clone()]);
        let classified = classified_for(&fixture);
        let progress = OverallProgress::start("test");

        let outcome = {
            let mut ctx = PullCtx {
                paths: &fixture.paths,
                client: &fixture.client,
                lockfile: &mut fixture.lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            resolve_conflicts(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b"r\n"),
                true,
                &progress,
            )
            .await
            .expect("resolver should succeed on [r]")
        };
        progress.finish();

        // No push-side promotion — remote wins, no PATCH needed.
        assert!(outcome.promoted_to_push.is_empty(), "no push items expected on [r]");

        // Local file overwritten with remote bytes.
        let remote_bytes = label_bytes(&fixture.remote_label);
        let local_after = std::fs::read(&fixture.local_path).unwrap();
        assert_eq!(local_after, remote_bytes, "local file should be replaced by remote bytes");

        // Lockfile records the remote hash.
        let recorded = fixture
            .lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get("audit-hold"))
            .and_then(|e| e.content_hash.clone())
            .unwrap();
        assert_eq!(recorded, content_hash(&remote_bytes));
    }

    /// Scripted `s\n` ([s]kip): the resolver writes a shadow file next
    /// to the local one, keeps the local file untouched, and records
    /// the local hash in the lockfile. No push promotion.
    #[tokio::test]
    async fn resolve_conflicts_skip_writes_shadow_and_keeps_local() {
        let mut fixture = setup_conflict_fixture();
        let catalog = catalog_with_labels(vec![fixture.remote_label.clone()]);
        let classified = classified_for(&fixture);
        let progress = OverallProgress::start("test");

        let local_before = std::fs::read(&fixture.local_path).unwrap();

        let outcome = {
            let mut ctx = PullCtx {
                paths: &fixture.paths,
                client: &fixture.client,
                lockfile: &mut fixture.lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            resolve_conflicts(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b"s\n"),
                true,
                &progress,
            )
            .await
            .expect("resolver should succeed on [s]")
        };
        progress.finish();

        assert!(outcome.promoted_to_push.is_empty(), "skip never promotes to push");

        // Shadow file at `<local>.<env>` carries the remote bytes.
        let shadow = crate::paths::shadow_path_for(&fixture.local_path, "test");
        assert!(shadow.exists(), "shadow file should be written at {}", shadow.display());
        let remote_bytes = label_bytes(&fixture.remote_label);
        assert_eq!(std::fs::read(&shadow).unwrap(), remote_bytes);

        // Local file untouched.
        let local_after = std::fs::read(&fixture.local_path).unwrap();
        assert_eq!(local_after, local_before, "local file must not be modified by [s]");

        // Lockfile records the LOCAL hash (skip means we keep our state).
        let local_hash = content_hash(&local_before);
        let recorded = fixture
            .lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get("audit-hold"))
            .and_then(|e| e.content_hash.clone())
            .unwrap();
        assert_eq!(recorded, local_hash);
    }

    /// Scripted `a\n` ([a]bort): the resolver returns a `PullAborted`
    /// error so the outer driver can detect it and skip lockfile.save().
    #[tokio::test]
    async fn resolve_conflicts_abort_returns_pull_aborted_error() {
        let mut fixture = setup_conflict_fixture();
        let catalog = catalog_with_labels(vec![fixture.remote_label.clone()]);
        let classified = classified_for(&fixture);
        let progress = OverallProgress::start("test");

        let err = {
            let mut ctx = PullCtx {
                paths: &fixture.paths,
                client: &fixture.client,
                lockfile: &mut fixture.lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            resolve_conflicts(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b"a\n"),
                true,
                &progress,
            )
            .await
            .expect_err("abort must surface as an error")
        };
        progress.finish();

        // Sentinel type lets the outer push/pull runner suppress
        // lockfile.save() — mirrors the apply_pull_action contract.
        assert!(
            err.chain().any(|c| c.downcast_ref::<PullAborted>().is_some()),
            "expected PullAborted in error chain, got: {err:?}",
        );
    }

    /// No `BothDiverged` items → resolver is a fast no-op and never
    /// reads from stdin. Pins the cheap-empty-case contract so a
    /// conflict-free sync doesn't accidentally block on stdin.
    #[tokio::test]
    async fn resolve_conflicts_no_op_when_no_diverged_items() {
        let mut fixture = setup_conflict_fixture();
        let catalog = catalog_with_labels(vec![fixture.remote_label.clone()]);
        // Re-classify the item as Clean so resolve_conflicts has nothing
        // to do.
        let classified = vec![ClassifiedItem {
            kind: "labels".to_string(),
            slug: "audit-hold".to_string(),
            class: SyncClass::Clean,
            local_hash: None,
            remote_hash: None,
            base_hash: None,
        }];
        let progress = OverallProgress::start("test");
        let lf_before = fixture.lockfile.clone();

        let outcome = {
            let mut ctx = PullCtx {
                paths: &fixture.paths,
                client: &fixture.client,
                lockfile: &mut fixture.lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            // Empty stdin — if the resolver tried to read here, the
            // `[s]kip` fallback would fire; we assert no resolution
            // happened by checking the lockfile is unchanged.
            resolve_conflicts(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b""),
                true,
                &progress,
            )
            .await
            .expect("no-op resolver must succeed")
        };
        progress.finish();

        assert!(outcome.promoted_to_push.is_empty(), "no items to promote");
        assert_eq!(fixture.lockfile, lf_before, "lockfile must be untouched");
    }
}
