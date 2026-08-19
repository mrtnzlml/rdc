use super::common::{
    HashMode, PullAction, PullCtx, apply_pull_action, apply_pull_action_with, decide_pull_action,
    decide_pull_action_with, record_object,
};
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

/// The only accepted value of the manifest's optional `data` key. Its presence
/// is what opts a dataset into row-data versioning.
pub(crate) const MANUAL_DATA_VALUE: &str = "manual";

/// Whether a dataset's ROW DATA is versioned in the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataMode {
    /// Metadata only — the historical (and default) behavior: rdc never reads
    /// or writes this collection's rows.
    None,
    /// Rows are pulled to `data.jsonl`, versioned, and pushed authoritatively.
    Manual,
}

/// Read a dataset's row-data mode from its manifest. Absent manifest or absent
/// `data` key ⇒ [`DataMode::None`] (backward compatible). An unrecognised value
/// is a hard error: silently ignoring it would look exactly like rdc dropping
/// the user's opt-in.
pub(crate) fn read_data_mode(dataset_dir: &std::path::Path) -> Result<DataMode> {
    let path = dataset_dir.join(COLLECTION_MANIFEST);
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(DataMode::None);
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(DataMode::None);
    };
    match value.get("data") {
        None | Some(Value::Null) => Ok(DataMode::None),
        Some(Value::String(s)) if s == MANUAL_DATA_VALUE => Ok(DataMode::Manual),
        Some(other) => anyhow::bail!(
            "{}: unrecognised \"data\" value {other} in {COLLECTION_MANIFEST}. \
             The only accepted value is \"{MANUAL_DATA_VALUE}\" (row data versioned \
             in {}); remove the key for metadata-only.",
            dataset_dir.display(),
            crate::snapshot::mdh_data::DATA_FILE,
        ),
    }
}

/// Canonical on-disk bytes for a collection manifest, MERGING into whatever the
/// file already holds: `name` is refreshed from server truth while every other
/// key (notably the `data` opt-in) and the file's own key order are preserved.
///
/// Clobbering instead of merging is what would erase a hand-added flag on the
/// next pull. Unparseable existing bytes fall back to a fresh manifest.
/// Serialized as `to_vec_pretty` + newline, matching migrate's JSON writer so
/// a migrated manifest and a pulled one are byte-comparable.
pub(crate) fn manifest_bytes_merged(existing: Option<&[u8]>, name: &str) -> Result<Vec<u8>> {
    let mut obj = existing
        .and_then(|b| serde_json::from_slice::<Value>(b).ok())
        .and_then(|v| match v {
            Value::Object(o) => Some(o),
            _ => None,
        })
        .unwrap_or_default();
    obj.insert("name".to_string(), Value::String(name.to_string()));
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(obj))
        .context("serializing collection manifest")?;
    bytes.push(b'\n');
    Ok(bytes)
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
///   dataset that was never synced (stage 2); plus, for a `"data": "manual"`
///   dataset, local `data.jsonl` drift against the lockfile baseline
///   (stage 1b);
/// - would pull: an env collection with no local dataset dir (stage 3,
///   "new") and a previously-synced dataset whose collection is gone on the
///   env (stage 4 orphan prune), gated on a non-empty listing; plus, for a
///   manual dataset with no local `data.jsonl` yet, a row-data pull
///   (stage 3b).
///
/// NOT predicted: remote-side index BODY edits, or remote-side ROW edits, to
/// a collection that already exists locally — detecting either needs a
/// per-collection fetch. See [`plan_mdh_index_edits`], the network-backed
/// companion that closes both gaps. The executor's pull still applies them
/// either way; this is a bounded, documented gap, not a silent one.
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

        // Stage 1b: row-data drift for manual datasets — local `data.jsonl`
        // hash differs from the lockfile baseline → PATCH. Hashed RAW (never
        // through `content_hash`'s canonicalizer): a one-line JSONL file
        // parses as a single JSON value, so the canonical path would strip a
        // `modified_at` / `modifier` / `training_enabled` COLUMN at any
        // depth — those are ordinary export columns here, not Rossum
        // metadata. `push_dataset_data` and `pull_dataset_data` both hash
        // raw; this forecast must match or it disagrees with the real run.
        //
        // `unwrap_or(DataMode::None)`: this function is infallible by design
        // (`Vec`, not `Result`) and its callers depend on that shape, so a
        // malformed "data" flag is deliberately swallowed into "not manual"
        // here. The fallible companion `plan_mdh_index_edits` is what
        // surfaces the error to the user in the same dry-run pass — don't
        // "fix" this `unwrap_or` into a silent divergence from the real run.
        for slug in &remote_slugs {
            if read_data_mode(&paths.dataset_dir(slug)).unwrap_or(DataMode::None)
                != DataMode::Manual
            {
                continue;
            }
            let Ok(bytes) = std::fs::read(paths.dataset_data(slug)) else {
                continue;
            };
            let local_hash = crate::state::raw_content_hash(&bytes);
            let base = lockfile
                .objects
                .get("mdh_data")
                .and_then(|m| m.get(slug))
                .and_then(|e| e.content_hash.clone());
            if base.as_deref() != Some(local_hash.as_str()) {
                items.push(MdhPlanItem {
                    dir: MdhPlanDir::Push,
                    line: format!("mdh/{slug} data PATCH"),
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

    // Stage 3b: a manual dataset with no local row data yet → a pull would
    // create `data.jsonl`. This CANNOT also cover the brand-new-dataset case
    // from stage 3 above: such a dataset has no local dataset dir yet, so
    // `read_data_mode` finds no manifest and returns `DataMode::None`, which
    // fails this loop's `== DataMode::Manual` check regardless of what the
    // remote collection's manifest would say. A brand-new manual dataset's
    // row-data forecast only appears once a real pull has materialized its
    // local manifest — one dry-run later. Same `unwrap_or(DataMode::None)`
    // swallow as stage 1b, for the same reason (infallible-by-design; the
    // fallible companion surfaces errors).
    for slug in &remote_slugs {
        if read_data_mode(&paths.dataset_dir(slug)).unwrap_or(DataMode::None) == DataMode::Manual
            && !paths.dataset_data(slug).is_file()
        {
            items.push(MdhPlanItem {
                dir: MdhPlanDir::Pull,
                line: format!("mdh/{slug} data (new)"),
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

/// Forecast whether a real sync would PULL (overwrite) a collection's local
/// `indexes.json` with the env's index definitions — the remote-side index
/// BODY edit that [`plan_mdh`]'s no-network structural scan cannot see. Reuses
/// the executor's exact three-way decision ([`decide_pull_action`]) so the
/// preview matches the real run. Returns a plan item only for `Write` (a clean
/// overwrite); `NoChange` is a no-op and `KeepLocal`/`Conflict` are
/// local-edit / both-diverged states the push or resolver owns — not a pull. A
/// missing local file is `plan_mdh` stage 3's "(new)" case, handled there.
fn index_edit_item(
    slug: &str,
    ix_path: &std::path::Path,
    base_hash: Option<&str>,
    proposed: &[u8],
) -> Result<Option<MdhPlanItem>> {
    // A missing local file is a create, not an index-body edit — leave it to
    // `plan_mdh` stage 3 so it is not forecast twice.
    if !ix_path.is_file() {
        return Ok(None);
    }
    let (action, _remote_hash) = decide_pull_action(ix_path, base_hash, proposed)?;
    Ok(match action {
        PullAction::Write => Some(MdhPlanItem {
            dir: MdhPlanDir::Pull,
            line: format!("mdh/{slug} (index update)"),
        }),
        // NoChange: local already matches remote. KeepLocal / Conflict: the
        // local file diverged — a push or the resolver owns those, not a pull.
        PullAction::NoChange | PullAction::KeepLocal | PullAction::Conflict => None,
    })
}

/// Network-backed companion to [`plan_mdh`]: for each env collection that
/// already has a local dataset dir, fetch its index definitions and forecast a
/// pull when they would overwrite the local `indexes.json`. Closes `plan_mdh`'s
/// documented gap (remote index-body edits on a locally-present collection) at
/// the cost of one index-list pair per local collection — the same fetches the
/// real sync's pull performs. Collections with no local dir are left to
/// `plan_mdh` stage 3 ("(new)").
///
/// Also closes the row-data analogue of that same gap: for a `"data": "manual"`
/// dataset whose `data.jsonl` already exists locally, fetches the collection's
/// current rows and forecasts a pull when they would overwrite the file — a
/// remote-side row edit `plan_mdh`'s no-network scan cannot see. Costs one
/// `find_all` per manual dataset with a local `data.jsonl`; an unflagged
/// dataset, or a manual one with no local rows yet (`plan_mdh` stage 3b's
/// "(new)" case), makes no row-data network call at all.
///
/// Unlike `plan_mdh`, this function is fallible and propagates
/// `read_data_mode`'s error on a malformed `"data"` flag — it runs in the same
/// dry-run pass as a real sync would, so the preview must fail exactly like
/// the real run does, not go silent about a problem that stops the real thing.
pub(crate) async fn plan_mdh_index_edits(
    listed: &MdhListed,
    lockfile: &crate::state::Lockfile,
    paths: &crate::paths::Paths,
    progress: &Arc<Log>,
) -> Result<Vec<MdhPlanItem>> {
    let mut items = Vec::new();
    if !listed.available {
        return Ok(items);
    }
    // Slug every collection exactly as the executor / `plan_mdh` do (listing
    // order, unique dedup) so forecast slugs match the real run byte-for-byte.
    let mut used: HashSet<String> = HashSet::new();
    for c in &listed.collections {
        let slug = slugify_unique(&c.name, &used);
        used.insert(slug.clone());

        let ix_path = paths.dataset_dir(&slug).join("indexes.json");
        // No local dataset dir → `plan_mdh` stage 3 already forecasts "(new)";
        // don't fetch (nothing to compare against) or double-report.
        if !ix_path.is_file() {
            continue;
        }

        let set = fetch_index_set(&listed.client, &c.name, progress).await?;
        let proposed = proposed_index_bytes(&set)?;
        let base = lockfile
            .objects
            .get("mdh_indexes")
            .and_then(|m| m.get(&slug))
            .and_then(|e| e.content_hash.clone());
        if let Some(item) = index_edit_item(&slug, &ix_path, base.as_deref(), &proposed)? {
            items.push(item);
        }

        // Row-data forecast for manual datasets — the row-level analogue of
        // the index-edit forecast above. `?`, not `unwrap_or`: this function
        // is fallible and runs in the same dry-run pass as a real sync, so a
        // malformed "data" flag must fail the preview exactly like it fails
        // the real run (see the doc comment on this function).
        if read_data_mode(&paths.dataset_dir(&slug))? == DataMode::Manual {
            let data_path = paths.dataset_data(&slug);
            // No local `data.jsonl` yet → `plan_mdh` stage 3b already
            // forecasts "(new)"; don't fetch (nothing to compare against)
            // or double-report. This is also what keeps an unflagged
            // dataset's cost unchanged: no manual opt-in, no fetch, ever.
            if data_path.is_file() {
                let rows = listed.client.find_all(&c.name, Some(progress.clone())).await?;
                let proposed = crate::snapshot::mdh_data::to_jsonl(&rows)?;
                let base = lockfile
                    .objects
                    .get("mdh_data")
                    .and_then(|m| m.get(&slug))
                    .and_then(|e| e.content_hash.clone());
                // `HashMode::Raw`: same reasoning as the pull driver at
                // `pull_dataset_data` — the bytes ARE the artifact, so
                // hashing must be verbatim, never through the canonical
                // JSON path that strips ordinary export columns.
                let (action, _) =
                    decide_pull_action_with(&data_path, base.as_deref(), &proposed, HashMode::Raw)?;
                if action == PullAction::Write {
                    items.push(MdhPlanItem {
                        dir: MdhPlanDir::Pull,
                        line: format!("mdh/{slug} data (update)"),
                    });
                }
            }
        }
    }
    Ok(items)
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

/// Serialize a fetched index set into the exact on-disk `indexes.json` bytes a
/// pull would write: server-managed fields stripped (`_id_`, `v`, search-index
/// status envelope), name-sorted, then run through the `mdh` codec. The single
/// source of truth for both the pull write (`process`) and the dry-run forecast
/// (`plan_mdh_index_edits`) — sharing it is what keeps the preview from ever
/// disagreeing with what the real sync would actually write.
fn proposed_index_bytes(set: &IndexSet) -> Result<Vec<u8>> {
    let trimmed = strip_server_managed(set);
    let value = serde_json::to_value(&trimmed).context("serializing index set as value")?;
    let art = crate::snapshot::codec::codec(KIND)
        .expect("mdh codec is registered")
        .disk_bytes(&value)
        .context("serializing index set via codec")?;
    Ok(art.json)
}

/// Fetch a collection's regular + search index definitions from the env.
/// Shared by the pull write path and the dry-run index-edit forecast.
async fn fetch_index_set(
    client: &DataStorageClient,
    collection_name: &str,
    progress: &Arc<Log>,
) -> Result<IndexSet> {
    let regular = client
        .list_indexes(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("listing indexes for '{collection_name}'"))?;
    let search = client
        .list_search_indexes(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("listing search indexes for '{collection_name}'"))?;
    Ok(IndexSet { regular, search })
}

/// Pull one manual dataset's rows into `data.jsonl`.
///
/// Returns `(changed, conflicts)`. Costs two calls (`$count` for the guardrail,
/// then one `find`) and is invoked ONLY for datasets flagged `"data": "manual"`,
/// so a metadata-only dataset stays exactly as cheap as it is today.
pub(crate) async fn pull_dataset_data(
    ctx: &mut PullCtx<'_>,
    client: &DataStorageClient,
    collection_name: &str,
    slug: &str,
    progress: &Arc<Log>,
) -> Result<(bool, usize)> {
    use crate::snapshot::mdh_data::{ROW_HARD_LIMIT, ROW_WARN_THRESHOLD, to_jsonl};

    // Guardrail first: refuse an oversized collection BEFORE reading it, so a
    // mis-flagged import-fed dataset can never be dragged into the snapshot.
    let count = client
        .count_documents(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("counting rows of '{collection_name}'"))?;
    if count > ROW_HARD_LIMIT {
        anyhow::bail!(
            "mdh/{slug}: '{collection_name}' holds {count} rows, over rdc's \
             {ROW_HARD_LIMIT}-row ceiling for versioned MDH data. Remove the \"data\" key \
             from {}/{COLLECTION_MANIFEST} to stop versioning this dataset's rows \
             (its name and indexes stay managed).",
            ctx.paths.dataset_dir(slug).display(),
        );
    }
    if count > ROW_WARN_THRESHOLD {
        progress.event(
            Action::Warn,
            &format!(
                "mdh/{slug}: {count} rows is large for a git-versioned dataset \
                 (warns above {ROW_WARN_THRESHOLD})"
            ),
        );
    }

    let rows = client
        .find_all(collection_name, Some(progress.clone()))
        .await
        .with_context(|| format!("reading rows of '{collection_name}'"))?;

    // An import-fed dataset carries the import extension's per-row digest.
    // Versioning it means rdc's authoritative pushes fight that hook for
    // ownership of the rows — warn, but do what the user asked.
    if rows.iter().any(|r| r.get("__digest_md5").is_some()) {
        progress.event(
            Action::Warn,
            &format!(
                "mdh/{slug}: rows carry __digest_md5, so this dataset looks maintained by \
                 an MDH import hook. rdc will treat data.jsonl as authoritative and may \
                 undo the hook's writes."
            ),
        );
    }

    let proposed = to_jsonl(&rows)?;
    let data_path = ctx.paths.dataset_data(slug);
    let base = ctx
        .lockfile
        .objects
        .get("mdh_data")
        .and_then(|m| m.get(slug))
        .and_then(|e| e.content_hash.clone());
    // `HashMode::Raw`: the bytes ARE the artifact (customer row data, not a
    // Rossum object) — see `HashMode` for why the canonical hash and the
    // format-migration nudge both misfire on JSONL.
    let (action, remote_hash) =
        decide_pull_action_with(&data_path, base.as_deref(), &proposed, HashMode::Raw)?;
    let conflicts = usize::from(action == PullAction::Conflict);

    // KeepLocal: the local file diverged while the remote matched base. MDH
    // bypasses the classifier, so this pull runs right AFTER the push every
    // cycle — landing here means the push did NOT reconcile (a gated delete
    // skip, `--no-push`, a vanished row). Advancing base to the LOCAL content
    // would starve the next cycle's push gate and make the cycle after revert
    // the file. Preserve the prior base so the push retries until it lands.
    // Mirrors the identical rule on the indexes leg.
    let recorded = if action == PullAction::KeepLocal {
        base.clone().unwrap_or(remote_hash)
    } else {
        // `HashMode::Raw`: a conflict resolution here (shadow fallback,
        // interactive keep-local/edit) must hash `data.jsonl` verbatim, same
        // as the decision above — see `HashMode` for why the canonical hash
        // misfires on JSONL row data.
        apply_pull_action_with(
            action,
            &data_path,
            &proposed,
            remote_hash,
            ctx.interactive,
            progress,
            ctx.paths.env(),
            base.as_deref(),
            Some(ctx.paths),
            HashMode::Raw,
        )?
    };
    record_object(ctx.lockfile, "mdh_data", slug, 0, None, Some(recorded));

    Ok((
        matches!(action, PullAction::Write | PullAction::Conflict),
        conflicts,
    ))
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
    // Slugs whose local representation actually changed THIS cycle (manifest
    // (re)written or indexes.json pulled). MDH bypasses the classifier and
    // this runs over every dataset each cycle, so the reported count must be
    // changed-only — a `BTreeSet` de-dupes a dataset that changed both files.
    let mut changed: BTreeSet<String> = BTreeSet::new();

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
        // identity) — idempotent. `manifest_bytes_merged` MERGES rather than
        // overwrites, so a legacy full-metadata `collection.json` a
        // pre-manifest project carried (`type` / `options` / `info` /
        // `idIndex`) is NOT cleaned up here: those keys survive every pull
        // indefinitely and travel through `migrate` into other envs.
        let manifest_path = dataset_dir.join(COLLECTION_MANIFEST);
        let existing = std::fs::read(&manifest_path).ok();
        let manifest_bytes = manifest_bytes_merged(existing.as_deref(), &c.name)?;
        let needs_write = existing.as_deref() != Some(manifest_bytes.as_slice());
        if needs_write {
            std::fs::write(&manifest_path, &manifest_bytes)
                .with_context(|| format!("writing {}", manifest_path.display()))?;
            changed.insert(slug.clone());
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
            let set = fetch_index_set(client_ref, &name, &progress).await?;
            Ok::<_, anyhow::Error>((slug, set))
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
    for (slug, dataset_dir, c) in &dataset_dirs {
        let Some(index_set) = by_slug.get(slug) else {
            continue;
        };
        let ix_action: PullAction = (|| -> Result<PullAction> {
            let ix_path = dataset_dir.join("indexes.json");

            // Serialize to the exact on-disk bytes a pull would write.
            let ix_proposed = proposed_index_bytes(index_set)?;

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
            Ok(i_action)
        })()?;
        // `Write` overwrote the local indexes.json; `Conflict` wrote merged
        // bytes / a shadow. `NoChange` and `KeepLocal` left the file alone.
        if matches!(ix_action, PullAction::Write | PullAction::Conflict) {
            changed.insert(slug.clone());
        }

        // Row data — only for datasets that opted in. `read_data_mode` errors
        // on a malformed flag, which surfaces here rather than being ignored.
        // The collection name comes from `c` (this loop's bound `Collection`,
        // server truth) rather than re-reading the manifest with a
        // directory-name fallback: `slugify` is lossy, so a fallback could
        // silently target a DIFFERENT collection than this dataset represents.
        if read_data_mode(dataset_dir)? == DataMode::Manual {
            let (data_changed, data_conflicts) =
                pull_dataset_data(ctx, &client, &c.name, slug, progress).await?;
            conflicts += data_conflicts;
            if data_changed {
                changed.insert(slug.clone());
            }
        }
    }

    // Report datasets that actually changed this cycle — NOT the total
    // processed. MDH runs over every dataset each cycle, so a no-op re-pull
    // must stay silent (matching the classifier-driven drivers) instead of
    // claiming it pulled everything.
    let changed_count = changed.len();
    if changed_count > 0 {
        progress.event(
            Action::Pull,
            &format!("mdh_datasets ({changed_count} pulled)"),
        );
    }

    Ok((changed_count, conflicts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_bytes_for_a_fresh_dataset_are_unchanged_from_the_legacy_form() {
        // Byte-identity with the pre-merge writer: an existing project must not
        // see churn in its collection.json files.
        assert_eq!(
            manifest_bytes_merged(None, "GL_CODES").unwrap(),
            b"{\n  \"name\": \"GL_CODES\"\n}\n"
        );
    }

    #[test]
    fn manifest_merge_preserves_the_manual_flag_while_refreshing_name() {
        // The pull rewrites `name` from server truth; every OTHER key the user
        // added must survive, or the opt-in flag would be erased on next pull.
        let existing = b"{\n  \"name\": \"OLD_NAME\",\n  \"data\": \"manual\"\n}\n";
        let merged = manifest_bytes_merged(Some(existing), "GL_CODES").unwrap();
        let v: Value = serde_json::from_slice(&merged).unwrap();
        assert_eq!(v["name"], serde_json::json!("GL_CODES"));
        assert_eq!(v["data"], serde_json::json!("manual"));
        assert_eq!(merged.last(), Some(&b'\n'), "trailing newline required");
    }

    #[test]
    fn manifest_merge_is_idempotent() {
        let once = manifest_bytes_merged(
            Some(b"{\n  \"name\": \"GL_CODES\",\n  \"data\": \"manual\"\n}\n"),
            "GL_CODES",
        )
        .unwrap();
        let twice = manifest_bytes_merged(Some(&once), "GL_CODES").unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn manifest_merge_tolerates_unparseable_existing_bytes() {
        // A corrupt manifest must not wedge the pull: fall back to a fresh one.
        assert_eq!(
            manifest_bytes_merged(Some(b"not json"), "GL_CODES").unwrap(),
            b"{\n  \"name\": \"GL_CODES\"\n}\n"
        );
    }

    #[test]
    fn read_data_mode_defaults_to_none_and_reads_manual() {
        let dir = tempfile::tempdir().unwrap();
        // No manifest at all → not manual (today's behavior).
        assert_eq!(read_data_mode(dir.path()).unwrap(), DataMode::None);

        std::fs::write(
            dir.path().join(COLLECTION_MANIFEST),
            b"{\n  \"name\": \"GL_CODES\"\n}\n",
        )
        .unwrap();
        assert_eq!(read_data_mode(dir.path()).unwrap(), DataMode::None);

        std::fs::write(
            dir.path().join(COLLECTION_MANIFEST),
            b"{\n  \"name\": \"GL_CODES\",\n  \"data\": \"manual\"\n}\n",
        )
        .unwrap();
        assert_eq!(read_data_mode(dir.path()).unwrap(), DataMode::Manual);
    }

    #[test]
    fn read_data_mode_rejects_an_unknown_value_loudly() {
        // A typo must fail loudly rather than silently reverting the dataset to
        // metadata-only (which would look like rdc ignoring the user).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(COLLECTION_MANIFEST),
            b"{\n  \"name\": \"GL_CODES\",\n  \"data\": \"Manual \"\n}\n",
        )
        .unwrap();
        let err = format!("{:#}", read_data_mode(dir.path()).unwrap_err());
        assert!(err.contains("Manual "), "must echo the bad value: {err}");
        assert!(err.contains("manual"), "must name the accepted value: {err}");
        assert!(err.contains(COLLECTION_MANIFEST), "must name the file: {err}");
    }

    #[test]
    fn read_collection_name_still_tolerates_absent_and_malformed() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_collection_name(dir.path()), None);
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

    /// MDH bypasses the classifier, so `--dry-run` is blind unless the planner
    /// mirrors the executor. Row data must be forecast too: a drifted
    /// data.jsonl is a would-push; a manual dataset with no local rows yet is
    /// a would-pull.
    #[test]
    fn plan_mdh_forecasts_row_data_deltas() {
        use crate::state::{Lockfile, ObjectEntry, content_hash};

        let root = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(root.path(), "dev");
        std::fs::create_dir_all(paths.mdh_dir()).unwrap();

        let ix = b"{\n  \"regular\": [],\n  \"search\": []\n}\n".to_vec();
        // `drifted`: manual, local rows differ from the baseline  -> data push.
        // `fresh`:   manual, no local rows at all                 -> data pull.
        // `plain`:   NOT manual                                   -> no data item.
        for slug in ["drifted", "fresh", "plain"] {
            let dir = paths.dataset_dir(slug);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("indexes.json"), &ix).unwrap();
            let manifest = if slug == "plain" {
                b"{\n  \"name\": \"P\"\n}\n".to_vec()
            } else {
                b"{\n  \"name\": \"M\",\n  \"data\": \"manual\"\n}\n".to_vec()
            };
            std::fs::write(dir.join(COLLECTION_MANIFEST), manifest).unwrap();
        }
        std::fs::write(paths.dataset_data("drifted"), b"{\"code\":\"1000\"}\n").unwrap();

        let entry = |h: &str| ObjectEntry {
            id: 0,
            modified_at: None,
            content_hash: Some(h.to_string()),
            secrets_hash: None,
        };
        let mut lf = Lockfile::default();
        let ix_hash = content_hash(&ix, &Lockfile::default());
        let mut ixs = std::collections::BTreeMap::new();
        for slug in ["drifted", "fresh", "plain"] {
            ixs.insert(slug.to_string(), entry(&ix_hash));
        }
        lf.objects.insert("mdh_indexes".to_string(), ixs);
        let mut data = std::collections::BTreeMap::new();
        data.insert("drifted".to_string(), entry("0000stalehash0000"));
        lf.objects.insert("mdh_data".to_string(), data);

        let collections = ["drifted", "fresh", "plain"]
            .iter()
            .map(|n| Collection { name: n.to_string(), extra: Default::default() })
            .collect();
        let listed = MdhListed {
            client: DataStorageClient::new("http://unused.invalid".to_string(), "t".to_string())
                .unwrap(),
            collections,
            available: true,
        };

        let lines: Vec<String> =
            plan_mdh(&listed, &lf, &paths, false).into_iter().map(|i| i.line).collect();
        assert!(
            lines.contains(&"mdh/drifted data PATCH".to_string()),
            "drifted rows must be forecast as a push: {lines:?}"
        );
        assert!(
            lines.contains(&"mdh/fresh data (new)".to_string()),
            "a manual dataset with no local rows must be forecast as a pull: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.starts_with("mdh/plain data")),
            "a non-manual dataset must produce no data forecast: {lines:?}"
        );

        // --no-push keeps the pull-side forecast, drops the push-side one.
        let np: Vec<String> =
            plan_mdh(&listed, &lf, &paths, true).into_iter().map(|i| i.line).collect();
        assert!(np.contains(&"mdh/fresh data (new)".to_string()));
        assert!(!np.iter().any(|l| l.contains("data PATCH")));
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

    /// MDH bypasses the classifier, so `process` runs over EVERY dataset on
    /// every cycle. Its `mdh_datasets (N pulled)` count must therefore report
    /// datasets actually changed this cycle — not the total processed — or a
    /// no-op re-pull falsely claims "N pulled". First pull materializes the
    /// dataset (1 changed); an immediate identical re-pull changes nothing (0).
    #[tokio::test]
    async fn process_counts_only_changed_datasets() {
        use crate::state::Lockfile;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/indexes/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [
                    { "name": "_id_", "key": { "_id": 1 }, "v": 2 },
                    { "name": "vendors_name_idx", "key": { "name": 1 }, "v": 2 }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search_indexes/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.mdh_dir()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        // `ctx.client` (RossumClient) is unused by the MDH pull — the
        // DataStorageClient in `listed` is what talks to the mock server.
        let rossum = crate::api::RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "t".to_string(),
        )
        .unwrap();
        let mut lockfile = Lockfile::default();
        let subset: BTreeSet<(String, String)> =
            [("mdh".to_string(), "vendors".to_string())].into_iter().collect();
        let mk_listed = || MdhListed {
            client: DataStorageClient::new(server.uri(), "t".to_string()).unwrap(),
            collections: vec![Collection {
                name: "vendors".to_string(),
                extra: Default::default(),
            }],
            available: true,
        };

        let (changed1, conflicts1) = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &rossum,
                lockfile: &mut lockfile,
                queue_locations: std::collections::BTreeMap::new(),
                interactive: false,
            };
            process(&mut ctx, mk_listed(), &subset, &progress).await.unwrap()
        };
        assert_eq!(conflicts1, 0);
        assert_eq!(changed1, 1, "first pull writes the dataset → 1 changed");

        let (changed2, _c2) = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &rossum,
                lockfile: &mut lockfile,
                queue_locations: std::collections::BTreeMap::new(),
                interactive: false,
            };
            process(&mut ctx, mk_listed(), &subset, &progress).await.unwrap()
        };
        assert_eq!(
            changed2, 0,
            "a no-op re-pull must report 0 changed, not the dataset total"
        );
    }

    /// The pure three-way core of the index-edit forecast. A remote index body
    /// that differs from an unedited local file (local == base) → the pull
    /// would overwrite it → forecast a pull. Identical remote → no forecast.
    /// Missing local file → no forecast (that is `plan_mdh` stage 3's "new").
    #[test]
    fn index_edit_item_flags_remote_index_body_edit() {
        use crate::state::{Lockfile, content_hash};
        let tmp = tempfile::tempdir().unwrap();
        let ix_path = tmp.path().join("indexes.json");
        let local = b"{\n  \"regular\": [ { \"name\": \"acct_v2\" } ],\n  \"search\": []\n}\n";
        std::fs::write(&ix_path, local).unwrap();
        let base = content_hash(local, &Lockfile::default());

        // Remote renamed the index; local is unedited (== base) → clean pull.
        let proposed = b"{\n  \"regular\": [ { \"name\": \"acct\" } ],\n  \"search\": []\n}\n";
        let item = index_edit_item("gl-codes", &ix_path, Some(&base), proposed).unwrap();
        assert_eq!(
            item.map(|i| (i.dir, i.line)),
            Some((MdhPlanDir::Pull, "mdh/gl-codes (index update)".to_string())),
            "a remote index-body edit on an unedited local file must be forecast as a pull"
        );

        // Remote identical to local → nothing to pull.
        assert!(
            index_edit_item("gl-codes", &ix_path, Some(&base), local)
                .unwrap()
                .is_none(),
            "identical remote must not be forecast"
        );

        // No local file → stage 3 "(new)" territory, not an index update.
        assert!(
            index_edit_item("gone", &tmp.path().join("nope.json"), Some(&base), proposed)
                .unwrap()
                .is_none(),
            "missing local file is not an index-update forecast"
        );
    }

    /// End-to-end: `plan_mdh_index_edits` fetches the env's index defs and
    /// forecasts a pull when they differ from an unedited local `indexes.json`.
    /// This is the gap `plan_mdh` alone (no network) cannot see.
    #[tokio::test]
    async fn plan_mdh_index_edits_forecasts_remote_index_change() {
        use crate::state::{Lockfile, content_hash};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Env now advertises index name "acct"; local snapshot has "acct_v2".
        Mock::given(method("POST"))
            .and(path("/v1/indexes/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [ { "name": "acct", "key": { "accountName": 1 } } ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search_indexes/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let dir = paths.dataset_dir("gl-codes");
        std::fs::create_dir_all(&dir).unwrap();
        // Local snapshot carries the OLD index name; record it as the base so
        // the file reads as unedited (local == base) → a clean pull.
        let local = b"{\n  \"regular\": [\n    {\n      \"key\": {\n        \"accountName\": 1\n      },\n      \"name\": \"acct_v2\"\n    }\n  ],\n  \"search\": []\n}\n";
        std::fs::write(dir.join("indexes.json"), local).unwrap();
        let mut lockfile = Lockfile::default();
        let mut mdh = std::collections::BTreeMap::new();
        mdh.insert(
            "gl-codes".to_string(),
            crate::state::ObjectEntry {
                id: 0,
                modified_at: None,
                content_hash: Some(content_hash(local, &Lockfile::default())),
                secrets_hash: None,
            },
        );
        lockfile.objects.insert("mdh_indexes".to_string(), mdh);

        let listed = MdhListed {
            client: DataStorageClient::new(server.uri(), "t".to_string()).unwrap(),
            collections: vec![Collection {
                name: "gl-codes".to_string(),
                extra: Default::default(),
            }],
            available: true,
        };
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);

        let items = plan_mdh_index_edits(&listed, &lockfile, &paths, &progress)
            .await
            .unwrap();
        let lines: Vec<(MdhPlanDir, String)> =
            items.into_iter().map(|i| (i.dir, i.line)).collect();
        assert_eq!(
            lines,
            vec![(MdhPlanDir::Pull, "mdh/gl-codes (index update)".to_string())],
            "a remote index-body edit must be forecast as a would-pull"
        );
    }

    /// Row-level analogue of `plan_mdh_index_edits_forecasts_remote_index_change`:
    /// a manual dataset whose local `data.jsonl` is unedited (its hash IS the
    /// lockfile base) but whose env rows have changed must be forecast as a
    /// would-pull, via the exact `HashMode::Raw` three-way decision the real
    /// pull applies. The index side is set up to fire NO item (fetched set ==
    /// local file, base recorded to match) so the row item is unambiguous.
    #[tokio::test]
    async fn plan_mdh_index_edits_forecasts_remote_row_data_update() {
        use crate::state::{Lockfile, content_hash, raw_content_hash};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Index side: env advertises the same (empty) index set the local
        // file already has — NoChange, no index item.
        Mock::given(method("POST"))
            .and(path("/v1/indexes/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search_indexes/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;
        // Row side: the env's row content changed.
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [ { "_id": { "$oid": "a1" }, "code": "9999", "label": "Changed" } ]
            })))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let dir = paths.dataset_dir("gl-codes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(COLLECTION_MANIFEST),
            b"{\n  \"name\": \"gl-codes\",\n  \"data\": \"manual\"\n}\n",
        )
        .unwrap();

        // Index fixture: the ACTUAL bytes `proposed_index_bytes` would produce
        // for an empty fetched set, so a fetch that also returns empty matches
        // it byte-for-byte and reads NoChange regardless of what base records.
        let empty_ix =
            proposed_index_bytes(&IndexSet { regular: vec![], search: vec![] }).unwrap();
        std::fs::write(dir.join("indexes.json"), &empty_ix).unwrap();

        // Row fixture: local rows differ from the env's rows. The lockfile
        // base is the LOCAL bytes' raw hash, so the file reads as unedited —
        // the only thing that changed is the remote.
        let local_rows: &[u8] = b"{\"code\":\"1000\",\"label\":\"Old\"}\n";
        std::fs::write(paths.dataset_data("gl-codes"), local_rows).unwrap();

        let mut lockfile = Lockfile::default();
        let mut ixs = std::collections::BTreeMap::new();
        ixs.insert(
            "gl-codes".to_string(),
            crate::state::ObjectEntry {
                id: 0,
                modified_at: None,
                content_hash: Some(content_hash(&empty_ix, &Lockfile::default())),
                secrets_hash: None,
            },
        );
        lockfile.objects.insert("mdh_indexes".to_string(), ixs);
        let mut data = std::collections::BTreeMap::new();
        data.insert(
            "gl-codes".to_string(),
            crate::state::ObjectEntry {
                id: 0,
                modified_at: None,
                content_hash: Some(raw_content_hash(local_rows)),
                secrets_hash: None,
            },
        );
        lockfile.objects.insert("mdh_data".to_string(), data);

        let listed = MdhListed {
            client: DataStorageClient::new(server.uri(), "t".to_string()).unwrap(),
            collections: vec![Collection {
                name: "gl-codes".to_string(),
                extra: Default::default(),
            }],
            available: true,
        };
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);

        let items = plan_mdh_index_edits(&listed, &lockfile, &paths, &progress)
            .await
            .unwrap();
        let lines: Vec<(MdhPlanDir, String)> =
            items.into_iter().map(|i| (i.dir, i.line)).collect();
        assert_eq!(
            lines,
            vec![(MdhPlanDir::Pull, "mdh/gl-codes data (update)".to_string())],
            "a remote row edit on an unedited local file must be forecast as a \
             would-pull, and nothing else"
        );
    }

    /// A manual dataset with NO local `data.jsonl` must trigger no row fetch at
    /// all: that case belongs to `plan_mdh` stage 3b's "(new)", and fetching
    /// (or forecasting) it here too would double-report it. `/v1/data/find` is
    /// deliberately NOT mounted — any row fetch would 404 and fail this test,
    /// which is what proves the gate prevents the request rather than merely
    /// producing output that happens to look right.
    #[tokio::test]
    async fn plan_mdh_index_edits_skips_row_fetch_when_no_local_data_file() {
        use crate::state::{Lockfile, content_hash};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/indexes/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search_indexes/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;
        // /v1/data/find is deliberately NOT mounted: any row fetch would 404.

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let dir = paths.dataset_dir("gl-codes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(COLLECTION_MANIFEST),
            b"{\n  \"name\": \"gl-codes\",\n  \"data\": \"manual\"\n}\n",
        )
        .unwrap();

        let empty_ix =
            proposed_index_bytes(&IndexSet { regular: vec![], search: vec![] }).unwrap();
        std::fs::write(dir.join("indexes.json"), &empty_ix).unwrap();
        // No data.jsonl written at all — the gate this test is proving.

        let mut lockfile = Lockfile::default();
        let mut ixs = std::collections::BTreeMap::new();
        ixs.insert(
            "gl-codes".to_string(),
            crate::state::ObjectEntry {
                id: 0,
                modified_at: None,
                content_hash: Some(content_hash(&empty_ix, &Lockfile::default())),
                secrets_hash: None,
            },
        );
        lockfile.objects.insert("mdh_indexes".to_string(), ixs);

        let listed = MdhListed {
            client: DataStorageClient::new(server.uri(), "t".to_string()).unwrap(),
            collections: vec![Collection {
                name: "gl-codes".to_string(),
                extra: Default::default(),
            }],
            available: true,
        };
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);

        let items = plan_mdh_index_edits(&listed, &lockfile, &paths, &progress)
            .await
            .unwrap();
        assert!(
            items.is_empty(),
            "no local data.jsonl must forecast no row item, and cost no row fetch: {items:?}"
        );
    }

    /// A dataset with no `"data": "manual"` flag must cost ZERO row calls — the
    /// backward-compatibility guarantee for every existing project.
    #[tokio::test]
    async fn process_makes_no_row_calls_for_a_non_manual_dataset() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/indexes/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/search_indexes/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "code": "ok", "result": [] })),
            )
            .mount(&server)
            .await;
        // /v1/data/find and /v1/data/aggregate are deliberately NOT mounted:
        // any row call would 404 and fail the pull.

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.mdh_dir()).unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let rossum = crate::api::RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "t".to_string(),
        )
        .unwrap();
        let mut lockfile = crate::state::Lockfile::default();
        let subset: BTreeSet<(String, String)> =
            [("mdh".to_string(), "gl-codes".to_string())].into_iter().collect();
        let listed = MdhListed {
            client: DataStorageClient::new(server.uri(), "t".to_string()).unwrap(),
            collections: vec![Collection {
                name: "gl-codes".to_string(),
                extra: Default::default(),
            }],
            available: true,
        };
        let mut ctx = PullCtx {
            paths: &paths,
            client: &rossum,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            interactive: false,
        };
        process(&mut ctx, listed, &subset, &progress).await.unwrap();
        assert!(
            !paths.dataset_data("gl-codes").exists(),
            "a non-manual dataset must get no data.jsonl"
        );
        assert!(!lockfile.objects.contains_key("mdh_data"));
    }

    /// A manual dataset's rows land in canonical form, and the lockfile +
    /// base cache record them so the next cycle is a no-op.
    #[tokio::test]
    async fn pull_dataset_data_writes_canonical_rows_and_records_state() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": [{ "n": 2 }] }),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [
                    { "_id": { "$oid": "a2" }, "label": "Travel", "code": "2000" },
                    { "_id": { "$oid": "a1" }, "label": "Office supplies", "code": "1000" }
                ]
            })))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let dir = paths.dataset_dir("gl-codes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(COLLECTION_MANIFEST),
            b"{\n  \"name\": \"GL_CODES\",\n  \"data\": \"manual\"\n}\n",
        )
        .unwrap();

        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let rossum = crate::api::RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "t".to_string(),
        )
        .unwrap();
        let client = DataStorageClient::new(server.uri(), "t".to_string()).unwrap();
        let mut lockfile = crate::state::Lockfile::default();
        let mut ctx = PullCtx {
            paths: &paths,
            client: &rossum,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            interactive: false,
        };

        let (changed, conflicts) =
            pull_dataset_data(&mut ctx, &client, "GL_CODES", "gl-codes", &progress)
                .await
                .unwrap();
        assert!(changed);
        assert_eq!(conflicts, 0);
        // Server ids stripped, keys sorted, lines sorted.
        assert_eq!(
            std::fs::read_to_string(paths.dataset_data("gl-codes")).unwrap(),
            "{\"code\":\"1000\",\"label\":\"Office supplies\"}\n\
             {\"code\":\"2000\",\"label\":\"Travel\"}\n"
        );
        // Via `ctx.lockfile`, not the outer `lockfile` binding: `ctx` still
        // holds lockfile's mutable borrow for the second call below, so a
        // fresh immutable borrow of `lockfile` here would conflict with it.
        assert!(ctx.lockfile.objects["mdh_data"].contains_key("gl-codes"));

        // Second pull over identical remote state changes nothing.
        let (changed2, _) =
            pull_dataset_data(&mut ctx, &client, "GL_CODES", "gl-codes", &progress)
                .await
                .unwrap();
        assert!(!changed2, "an unchanged re-pull must report no change");
    }

    #[tokio::test]
    async fn pull_dataset_data_refuses_a_collection_over_the_hard_limit() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [{ "n": crate::snapshot::mdh_data::ROW_HARD_LIMIT + 1 }]
            })))
            .mount(&server)
            .await;
        // /v1/data/find not mounted: the guardrail must refuse BEFORE reading.

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let dir = paths.dataset_dir("gl-codes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(COLLECTION_MANIFEST),
            b"{\n  \"name\": \"GL_CODES\",\n  \"data\": \"manual\"\n}\n",
        )
        .unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let rossum = crate::api::RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "t".to_string(),
        )
        .unwrap();
        let client = DataStorageClient::new(server.uri(), "t".to_string()).unwrap();
        let mut lockfile = crate::state::Lockfile::default();
        let mut ctx = PullCtx {
            paths: &paths,
            client: &rossum,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            interactive: false,
        };
        let err = pull_dataset_data(&mut ctx, &client, "GL_CODES", "gl-codes", &progress)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("10001"), "must state the count: {msg}");
        assert!(!paths.dataset_data("gl-codes").exists(), "must write nothing");
    }

    /// `KeepLocal` is the ONLY thing stopping a locally-edited `data.jsonl`
    /// from being silently reverted two cycles later: if base advanced to the
    /// LOCAL content here, the next cycle's push gate (`local_hash == base`)
    /// would see nothing to push, and the cycle after that would then see
    /// local == base / remote != base and overwrite the file with remote —
    /// reverting the user's edit. This proves base is left at its PRIOR
    /// value (not advanced to local) when remote matches base but local has
    /// diverged from it.
    #[tokio::test]
    async fn pull_dataset_data_keep_local_preserves_prior_base_hash() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/data/aggregate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": "ok", "result": [{ "n": 2 }] }),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/data/find"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok",
                "result": [
                    { "_id": { "$oid": "a2" }, "label": "Travel", "code": "2000" },
                    { "_id": { "$oid": "a1" }, "label": "Office supplies", "code": "1000" }
                ]
            })))
            .mount(&server)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_env(tmp.path(), "dev");
        let dir = paths.dataset_dir("gl-codes");
        std::fs::create_dir_all(&dir).unwrap();

        // What the remote proposes, canonical form — byte-identical to the
        // fixture in `pull_dataset_data_writes_canonical_rows_and_records_state`.
        let remote_proposed: &[u8] = b"{\"code\":\"1000\",\"label\":\"Office supplies\"}\n{\"code\":\"2000\",\"label\":\"Travel\"}\n";
        let base_hash = crate::state::raw_content_hash(remote_proposed);

        // Local has diverged from base (a real edit); remote still matches base.
        let local_edited: &[u8] = b"{\"code\":\"1000\",\"label\":\"Office supplies EDITED\"}\n{\"code\":\"2000\",\"label\":\"Travel\"}\n";
        std::fs::write(paths.dataset_data("gl-codes"), local_edited).unwrap();

        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let rossum = crate::api::RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "t".to_string(),
        )
        .unwrap();
        let client = DataStorageClient::new(server.uri(), "t".to_string()).unwrap();
        let mut lockfile = crate::state::Lockfile::default();
        record_object(&mut lockfile, "mdh_data", "gl-codes", 0, None, Some(base_hash.clone()));
        let mut ctx = PullCtx {
            paths: &paths,
            client: &rossum,
            lockfile: &mut lockfile,
            queue_locations: std::collections::BTreeMap::new(),
            interactive: false,
        };

        let (changed, conflicts) =
            pull_dataset_data(&mut ctx, &client, "GL_CODES", "gl-codes", &progress)
                .await
                .unwrap();
        assert!(!changed, "KeepLocal is not a change");
        assert_eq!(conflicts, 0);
        assert_eq!(
            std::fs::read(paths.dataset_data("gl-codes")).unwrap(),
            local_edited,
            "the pull must not overwrite the locally-edited file"
        );
        // The crux of this test: base must stay at its PRIOR value, never
        // advance to local. Asserting only "the file didn't change" would
        // still pass even if base HAD wrongly advanced — this is the
        // assertion that actually guards against period-2 churn.
        let recorded = ctx.lockfile.objects["mdh_data"]["gl-codes"].content_hash.clone();
        assert_eq!(
            recorded,
            Some(base_hash),
            "base must be preserved at its prior value, not advanced"
        );
        assert_ne!(
            recorded,
            Some(crate::state::raw_content_hash(local_edited)),
            "base must NOT advance to the local content"
        );
    }
}
