use super::common::{PullAction, PullCtx, apply_pull_action, decide_pull_action, record_object};
use crate::api::{DataStorageClient, anyhow_has_status};
use crate::config::EnvConfig;
use crate::log::{Action, Log};
use crate::model::{Collection, IndexSet};
use crate::slug::slugify_unique;
use anyhow::{Context, Result};
use futures::stream::{StreamExt, TryStreamExt};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

const KIND: &str = "mdh";

/// Per-dataset manifest filename. Persists the MDH collection's server
/// `name` — the single identity field that is NOT recoverable from the
/// on-disk dataset slug (`slugify` is lossy, e.g. `PO_CANCELS` and
/// `po cancels` both slug to `po-cancels`). Required to (re)create a
/// collection on an env that does not have it yet.
pub(crate) const COLLECTION_MANIFEST: &str = "collection.json";

/// Canonical on-disk bytes for a collection manifest: pretty JSON with a
/// trailing newline, matching every other snapshot file.
pub(crate) fn collection_manifest_bytes(name: &str) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({ "name": name }))
        .expect("serializing collection manifest (a single-key object never fails)");
    bytes.push(b'\n');
    bytes
}

/// Read a dataset's collection name from its manifest. Returns `None` when
/// the manifest is absent (a legacy dataset predating the manifest) or
/// unparseable — never panics; the caller warns and skips.
pub(crate) fn read_collection_name(dataset_dir: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(dataset_dir.join(COLLECTION_MANIFEST)).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("name")?.as_str().map(str::to_owned)
}

/// Local dataset slugs that have an `indexes.json` on disk but whose slug is
/// NOT in `remote_slugs` (their collection is absent on the env). These are
/// the datasets a deploy may need to create. Slugs present in `remote_slugs`
/// are handled by the remote-driven path and deliberately excluded here, so
/// this never touches an existing collection. Sorted for deterministic order.
pub(crate) fn local_only_dataset_slugs(
    mdh_dir: &std::path::Path,
    remote_slugs: &BTreeSet<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(mdh_dir) else {
        return out; // no local mdh dir → nothing to create
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(slug) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if remote_slugs.contains(&slug) {
            continue; // handled by the remote-driven path; never touched here
        }
        if entry.path().join("indexes.json").is_file() {
            out.push(slug);
        }
    }
    out.sort();
    out
}

/// Direction of a predicted MDH op, so the dry-run planner can file it under
/// the right section ("would pull" vs "would push").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MdhPlanDir {
    Pull,
    Push,
}

/// One MDH operation a real sync would perform, predicted purely from the
/// already-listed catalog + the lockfile + local files — with NO extra
/// network round-trips. The `line` is ready to render as `- {line}`.
#[derive(Debug, Clone)]
pub(crate) struct MdhPlanItem {
    pub dir: MdhPlanDir,
    /// Display line including the `mdh/<slug>` prefix plus the trailing note
    /// / action, e.g. `mdh/vendors (new)` or `mdh/vendors PATCH`.
    pub line: String,
}

/// Predict the MDH operations `sync` would perform, for `--dry-run`.
///
/// MDH bypasses the classifier (see `sync::execute`), so its would-be writes
/// never rode the dry-run plan — a preview reported `0 would pull` while a
/// real sync then created a whole new local dataset dir. This mirrors the
/// executor's MDH stages closely enough to preview the STRUCTURAL deltas with
/// no network beyond the collection listing the scan already fetched:
///
/// - would push (only when `!no_push`): local `indexes.json` drift on an
///   existing collection (executor stage 1) and a brand-new local-only
///   dataset that was never synced (stage 2);
/// - would pull: an env collection with no local dataset dir (stage 3,
///   "new") and a previously-synced dataset whose collection is gone on the
///   env (stage 4 orphan prune), gated on a non-empty listing.
///
/// NOT predicted: remote-side index BODY edits to a collection that already
/// exists locally — detecting those needs a per-collection index fetch. The
/// executor's pull still applies them; this is a bounded, documented gap, not
/// a silent one.
pub(crate) fn plan_mdh(
    listed: &MdhListed,
    lockfile: &crate::state::Lockfile,
    paths: &crate::paths::Paths,
    no_push: bool,
) -> Vec<MdhPlanItem> {
    let mut items = Vec::new();
    if !listed.available {
        return items;
    }

    // Slug every env collection exactly as the executor does (listing order,
    // unique dedup) so predicted slugs match the real run byte-for-byte.
    let mut used: HashSet<String> = HashSet::new();
    let mut remote_slugs: BTreeSet<String> = BTreeSet::new();
    for c in &listed.collections {
        let slug = slugify_unique(&c.name, &used);
        used.insert(slug.clone());
        remote_slugs.insert(slug);
    }

    let mdh_base = lockfile.objects.get("mdh_indexes");
    let base_hash = |slug: &str| -> Option<String> {
        mdh_base
            .and_then(|m| m.get(slug))
            .and_then(|e| e.content_hash.clone())
    };

    if !no_push {
        // Stage 1: index drift on an existing collection — local
        // `indexes.json` hash differs from the lockfile baseline → PATCH.
        for slug in &remote_slugs {
            let indexes_path = paths.dataset_dir(slug).join("indexes.json");
            let Ok(bytes) = std::fs::read(&indexes_path) else {
                continue;
            };
            let local_hash =
                crate::state::content_hash(&bytes, &crate::state::Lockfile::default());
            if base_hash(slug).as_deref() != Some(local_hash.as_str()) {
                items.push(MdhPlanItem {
                    dir: MdhPlanDir::Push,
                    line: format!("mdh/{slug} PATCH"),
                });
            }
        }

        // Stage 2: a local-only dataset absent on the env and never synced
        // (no lockfile entry) → POST (create the collection). A local-only
        // dataset that WAS synced is an orphan, left to stage 4's prune.
        for slug in local_only_dataset_slugs(&paths.mdh_dir(), &remote_slugs) {
            if base_hash(&slug).is_some() {
                continue;
            }
            items.push(MdhPlanItem {
                dir: MdhPlanDir::Push,
                line: format!("mdh/{slug} POST"),
            });
        }
    }

    // Stage 3 (structural pull): an env collection with no local
    // `indexes.json` → the pull would create the dataset dir locally.
    for slug in &remote_slugs {
        if !paths.dataset_dir(slug).join("indexes.json").is_file() {
            items.push(MdhPlanItem {
                dir: MdhPlanDir::Pull,
                line: format!("mdh/{slug} (new)"),
            });
        }
    }

    // Stage 4: orphan prune — a previously-synced dataset whose collection is
    // gone on the env. Gated on a NON-EMPTY listing, matching the executor's
    // guard against a transient empty/404 mass-deleting every local dataset.
    if !remote_slugs.is_empty()
        && let Some(m) = mdh_base
    {
        for slug in m.keys() {
            if !remote_slugs.contains(slug) {
                items.push(MdhPlanItem {
                    dir: MdhPlanDir::Pull,
                    line: format!("mdh/{slug} (delete local; deleted on env)"),
                });
            }
        }
    }

    items
}

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
        .filter(|ix| ix.get("name").and_then(|n| n.as_str()) != Some("_id_"))
        .cloned()
        .collect();
    for ix in regular.iter_mut() {
        if let Value::Object(obj) = ix {
            obj.shift_remove("v");
        }
    }
    let mut search: Vec<Value> = set
        .search
        .iter()
        .filter_map(normalize_search_index)
        .collect();
    // Canonically name-sort both lists. The Data Storage list endpoints return
    // indexes in an order that is unstable across environments; writing that raw
    // order to disk makes `migrate` (source order) and `sync`'s pull-leg (target
    // order) rewrite the same `indexes.json` on every run (a hash-flip ping-pong,
    // since array element order is significant to the content hash). A stable
    // name-sort makes the on-disk form deterministic and the workflow idempotent.
    sort_indexes_by_name(&mut regular);
    sort_indexes_by_name(&mut search);
    IndexSet { regular, search }
}

/// Sort an index list by the `name` field, with the full canonical JSON as a
/// tiebreaker so the order is total and deterministic even in the (unexpected)
/// case of a missing or duplicate name.
fn sort_indexes_by_name(list: &mut [Value]) {
    list.sort_by(|a, b| {
        let an = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let bn = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        an.cmp(bn).then_with(|| a.to_string().cmp(&b.to_string()))
    });
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
    /// Whether MDH is provisioned on this env. `true` when the collection
    /// listing succeeded (even with zero collections); `false` when the
    /// Data Storage endpoint 404s (MDH not enabled on the cluster). A 404
    /// and a genuinely-empty listing both yield `collections == []`, so this
    /// flag is the only way to tell them apart — the deploy uses it to gate
    /// collection creation (create on a fresh-but-enabled env; never attempt
    /// it against a cluster without MDH).
    pub available: bool,
}

/// Phase 1: list MDH collections (or return an empty list if MDH is not
/// enabled on this cluster — 404 → quiet skip matching the 403 pattern).
pub async fn list(env_cfg: &EnvConfig, token: &str, progress: &Arc<Log>) -> Result<MdhListed> {
    let base = env_cfg.data_storage_base();
    let client = DataStorageClient::new(base, token.to_string())
        .context("constructing Data Storage client")?;

    let (collections, available) = match client.list_collections(Some(progress.clone())).await {
        Ok(c) => (c, true),
        Err(e) if anyhow_has_status(&e, 404) => {
            // MDH not enabled on this cluster — quietly skip. `available:
            // false` keeps the deploy from attempting collection creation
            // against a cluster that has no Data Storage service.
            (Vec::new(), false)
        }
        Err(e) => return Err(e.context("listing MDH collections")),
    };

    Ok(MdhListed {
        client,
        collections,
        available,
    })
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
    let MdhListed {
        client,
        collections,
        available: _,
    } = listed;

    if collections.is_empty() {
        return Ok((0, 0));
    }

    let mut used: HashSet<String> = HashSet::new();
    let mut conflicts = 0usize;

    let mut dir_created = false;

    // === Sub-phase A: assign slugs, ensure dataset_dir exists, and persist
    //            each collection's `name` manifest (the one identity field
    //            the slug cannot recover). Also drops the redundant legacy
    //            `mdh_collections` lockfile entry. The indexes.json write
    //            itself happens in sub-phase C after the parallel fetches.
    let mut dataset_dirs: Vec<(String, std::path::PathBuf, Collection)> = Vec::new();
    for c in collections {
        let slug = slugify_unique(&c.name, &used);
        used.insert(slug.clone());

        if !subset.contains(&(KIND.to_string(), slug.clone())) {
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

        // Persist the collection's server `name` to a minimal manifest.
        // `slugify` is lossy, so the slug cannot recover the name; a deploy
        // to an env that lacks the collection needs it to (re)create the
        // collection. Written deterministically (server-authoritative
        // identity) — idempotent, and it also overwrites any legacy
        // full-metadata `collection.json` a pre-manifest project carried.
        let manifest_path = dataset_dir.join(COLLECTION_MANIFEST);
        let manifest_bytes = collection_manifest_bytes(&c.name);
        let needs_write = std::fs::read(&manifest_path)
            .map(|existing| existing != manifest_bytes)
            .unwrap_or(true);
        if needs_write {
            std::fs::write(&manifest_path, &manifest_bytes)
                .with_context(|| format!("writing {}", manifest_path.display()))?;
        }
        // Legacy: pre-manifest projects also carry a redundant
        // `mdh_collections.<slug>` lockfile entry. Drop it — the on-disk
        // manifest is the source of truth for the collection name now.
        if let Some(map) = ctx.lockfile.objects.get_mut("mdh_collections")
            && map.remove(&slug).is_some()
        {
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
        && map.is_empty()
    {
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
        dataset_dirs
            .iter()
            .map(|(slug, _, c)| (slug.clone(), c.name.clone())),
    )
    .map(|(slug, name)| {
        let progress = progress.clone();
        async move {
            let regular = client_ref
                .list_indexes(&name, Some(progress.clone()))
                .await
                .with_context(|| format!("listing indexes for '{name}'"))?;
            let search = client_ref
                .list_search_indexes(&name, Some(progress.clone()))
                .await
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
    //            user can actually edit. Hash via KindCodec (byte-identical
    //            to the legacy content_hash path since codec.disk_bytes for
    //            mdh produces the same bytes as the legacy strip+serialize).
    for (slug, dataset_dir, _c) in &dataset_dirs {
        let Some(index_set) = by_slug.get(slug) else {
            continue;
        };
        let trimmed = strip_server_managed(index_set);

        let ix_result: Result<()> = (|| {
            let ix_path = dataset_dir.join("indexes.json");

            // Use KindCodec for byte + hash production.
            let value = serde_json::to_value(&trimmed).context("serializing index set as value")?;
            let art = crate::snapshot::codec::codec(KIND)
                .unwrap()
                .disk_bytes(&value)
                .context("serializing index set via codec")?;
            let ix_proposed = art.json;

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
            // KeepLocal — the local file diverged, remote is unchanged from
            // base. Because MDH bypasses the classifier, this pull runs on
            // EVERY cycle right AFTER the push; landing here means the push
            // did NOT reconcile local → remote this cycle (an index create
            // that never materialized, `--no-push`, or a gated delete skip).
            // The local edit is therefore still PENDING. The generic
            // `apply_pull_action` would advance base to the LOCAL content —
            // which starves the next cycle's push gate (`local_hash == base`)
            // and makes the cycle after REVERT the file (period-2 churn).
            // Preserve the prior base instead: the push retries every cycle
            // until it actually reconciles (e.g. the user deduplicates the
            // data violating a unique index and the create finally builds).
            let i_recorded = if i_action == PullAction::KeepLocal {
                ix_base.clone().unwrap_or(i_remote_hash)
            } else {
                apply_pull_action(
                    i_action,
                    &ix_path,
                    &ix_proposed,
                    i_remote_hash,
                    ctx.interactive,
                    progress,
                    ctx.paths.env(),
                    ix_base.as_deref(),
                    Some(ctx.paths),
                )?
            };
            record_object(ctx.lockfile, "mdh_indexes", slug, 0, None, Some(i_recorded));
            Ok(())
        })();
        ix_result?;
    }

    if !dataset_dirs.is_empty() {
        progress.event(
            Action::Pull,
            &format!("mdh_datasets ({} pulled)", dataset_dirs.len()),
        );
    }

    Ok((dataset_dirs.len(), conflicts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_manifest_round_trips_name() {
        // The manifest persists the one identity field the on-disk slug
        // cannot recover (slugify is lossy). Bytes must be the canonical
        // pretty-JSON-plus-newline shared by every other snapshot file, and
        // reading them back must yield the exact name.
        let bytes = collection_manifest_bytes("PO_CANCELS");
        assert_eq!(bytes, b"{\n  \"name\": \"PO_CANCELS\"\n}\n");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(COLLECTION_MANIFEST), &bytes).unwrap();
        assert_eq!(read_collection_name(dir.path()).as_deref(), Some("PO_CANCELS"));
    }

    #[test]
    fn read_collection_name_tolerates_absent_and_malformed() {
        let dir = tempfile::tempdir().unwrap();
        // No manifest → None (a legacy dataset predating the manifest).
        assert_eq!(read_collection_name(dir.path()), None);
        // Malformed JSON → None (never panics; caller warns + skips).
        std::fs::write(dir.path().join(COLLECTION_MANIFEST), b"not json").unwrap();
        assert_eq!(read_collection_name(dir.path()), None);
    }

    #[test]
    fn local_only_dataset_slugs_excludes_remote_and_requires_indexes() {
        // `a` is local-only with indexes → a create candidate.
        // `b` exists remotely → handled by the remote-driven path, never here.
        // `c` has no indexes.json → not a dataset, skipped.
        let mdh = tempfile::tempdir().unwrap();
        for slug in ["a", "b", "c"] {
            std::fs::create_dir_all(mdh.path().join(slug)).unwrap();
        }
        std::fs::write(mdh.path().join("a/indexes.json"), b"{}").unwrap();
        std::fs::write(mdh.path().join("b/indexes.json"), b"{}").unwrap();

        let remote: BTreeSet<String> = ["b".to_string()].into_iter().collect();
        assert_eq!(local_only_dataset_slugs(mdh.path(), &remote), vec!["a".to_string()]);
    }

    #[test]
    fn local_only_dataset_slugs_missing_dir_is_empty() {
        // No mdh dir at all (env never had MDH) → nothing to create.
        let tmp = tempfile::tempdir().unwrap();
        let remote = BTreeSet::new();
        assert!(local_only_dataset_slugs(&tmp.path().join("nope"), &remote).is_empty());
    }

    /// `plan_mdh` must predict every STRUCTURAL MDH delta a real sync would
    /// perform — the four executor stages — purely from the listed catalog +
    /// lockfile + local files, with no network. This is what lets
    /// `sync --dry-run` preview MDH (which bypasses the classifier).
    #[test]
    fn plan_mdh_previews_all_structural_deltas() {
        use crate::state::{Lockfile, ObjectEntry, content_hash};

        let root = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(root.path(), "dev");
        std::fs::create_dir_all(paths.mdh_dir()).unwrap();

        // Local datasets on disk: `clean` (matches baseline), `drift` (local
        // edit vs baseline), `localcreate` (never synced, env-absent).
        let clean_bytes = b"{\n  \"regular\": [],\n  \"search\": []\n}\n".to_vec();
        let drift_bytes = b"{\n  \"regular\": [{ \"name\": \"x\" }],\n  \"search\": []\n}\n".to_vec();
        for (slug, bytes) in [
            ("clean", &clean_bytes),
            ("drift", &drift_bytes),
            ("localcreate", &clean_bytes),
        ] {
            let dir = paths.dataset_dir(slug);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("indexes.json"), bytes).unwrap();
        }

        // Lockfile baseline. `clean`'s hash matches on disk → no op. `drift`
        // records a stale hash → PATCH. `orphan` was synced before but has no
        // env collection and no local dir → delete-local. `localcreate` has NO
        // entry (never synced) → create.
        let entry = |h: &str| ObjectEntry {
            id: 0,
            modified_at: None,
            content_hash: Some(h.to_string()),
            secrets_hash: None,
        };
        let mut mdh = std::collections::BTreeMap::new();
        mdh.insert(
            "clean".to_string(),
            entry(&content_hash(&clean_bytes, &Lockfile::default())),
        );
        mdh.insert("drift".to_string(), entry("0000stalehash0000"));
        mdh.insert("orphan".to_string(), entry("0000orphanhash000"));
        let mut lf = Lockfile::default();
        lf.objects.insert("mdh_indexes".to_string(), mdh);

        // The env lists clean, drift, and a brand-new collection with no local
        // dir. `orphan` and `localcreate` are deliberately absent from the env.
        let collections = ["clean", "drift", "brandnew"]
            .iter()
            .map(|n| Collection {
                name: n.to_string(),
                extra: Default::default(),
            })
            .collect();
        let listed = MdhListed {
            client: DataStorageClient::new("http://unused.invalid".to_string(), "t".to_string())
                .unwrap(),
            collections,
            available: true,
        };

        let mut got: Vec<(MdhPlanDir, String)> = plan_mdh(&listed, &lf, &paths, false)
            .into_iter()
            .map(|i| (i.dir, i.line))
            .collect();
        got.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(
            got,
            vec![
                (MdhPlanDir::Pull, "mdh/brandnew (new)".to_string()),
                (MdhPlanDir::Push, "mdh/drift PATCH".to_string()),
                (MdhPlanDir::Push, "mdh/localcreate POST".to_string()),
                (
                    MdhPlanDir::Pull,
                    "mdh/orphan (delete local; deleted on env)".to_string()
                ),
            ],
            "must preview new/drift/create/orphan and skip the clean dataset"
        );

        // `--no-push` drops the push-side ops; the pulls survive.
        let np: Vec<String> = plan_mdh(&listed, &lf, &paths, true)
            .into_iter()
            .map(|i| i.line)
            .collect();
        assert!(np.contains(&"mdh/brandnew (new)".to_string()));
        assert!(np.contains(&"mdh/orphan (delete local; deleted on env)".to_string()));
        assert!(
            !np.iter().any(|l| l.contains("PATCH") || l.contains("POST")),
            "no-push must suppress MDH pushes: {np:?}"
        );
    }

    /// MDH not provisioned on the env (`available == false`) → an empty plan.
    /// A dry-run must never predict ops against a cluster without Data Storage.
    #[test]
    fn plan_mdh_unavailable_env_yields_no_plan() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(root.path(), "dev");
        let listed = MdhListed {
            client: DataStorageClient::new("http://unused.invalid".to_string(), "t".to_string())
                .unwrap(),
            collections: vec![Collection {
                name: "vendors".to_string(),
                extra: Default::default(),
            }],
            available: false,
        };
        assert!(plan_mdh(&listed, &crate::state::Lockfile::default(), &paths, false).is_empty());
    }

    #[test]
    fn strip_server_managed_sorts_indexes_by_name_deterministically() {
        use serde_json::json;
        // The Data Storage list endpoints return indexes in an unstable order
        // that differs across environments. Writing that raw order to disk makes
        // `migrate` (dev order) and `sync`'s pull-leg (target order) ping-pong
        // the same file forever. Canonically name-sorting the on-disk form is
        // what keeps the snapshot deterministic and the workflow idempotent.
        let set = IndexSet {
            regular: vec![
                json!({ "name": "b_idx", "key": { "b": 1 } }),
                json!({ "name": "a_idx", "key": { "a": 1 } }),
            ],
            search: vec![
                json!({ "name": "z_search", "mappings": { "dynamic": true } }),
                json!({ "name": "m_search", "mappings": { "dynamic": false } }),
            ],
        };
        let out = strip_server_managed(&set);
        let regular: Vec<&str> =
            out.regular.iter().map(|i| i["name"].as_str().unwrap()).collect();
        let search: Vec<&str> = out.search.iter().map(|i| i["name"].as_str().unwrap()).collect();
        assert_eq!(regular, vec!["a_idx", "b_idx"], "regular indexes must be name-sorted");
        assert_eq!(search, vec!["m_search", "z_search"], "search indexes must be name-sorted");
    }
}
