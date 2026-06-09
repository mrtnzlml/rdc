//! Execute the classified plan. Dispatches four branches:
//!
//! - **Conflict (`BothDiverged`)** — runs first via [`resolve_conflicts`].
//!   Each conflict prompts the user (`[k]/[r]/[e]/[s]/[a]`), then routes the
//!   outcome: keep-local / edit promote the item to LocalEdit (pushed
//!   below); keep-remote writes the remote bytes to disk + lockfile;
//!   skip writes a shadow file and records the local hash; abort bubbles
//!   [`crate::cli::resolve::PullAborted`].
//! - **Remote-delete + double-conflict + both-deleted** — handled by
//!   [`resolve_remote_deletes`]. `RemoteDelete`, `LocalEditRemoteDelete`,
//!   and `LocalDeleteRemoteEdit` share the same `[k]/[r]/[s]/[a]` prompt
//!   shape ([`crate::cli::resolve::prompt_remote_delete`]); `BothDeleted`
//!   converges silently by dropping the lockfile entry. `[k]` (restore on
//!   env) drops the lockfile entry and promotes the item to the push
//!   pipeline so it's POSTed; `[r]` mirrors the deletion locally; `[s]`
//!   writes the `<file>.<env>-deleted` marker.
//! - **Push-side (`LocalEdit`, `LocalCreate`)** — folded into a
//!   `ChangeList` via [`crate::cli::push::scan::change_list_from_classified`]
//!   and dispatched through the existing push pipeline. Items promoted
//!   from the conflict and remote-delete branches are merged into the
//!   same ChangeList so they take a single round-trip through the push
//!   driver. Push runs BEFORE pull so resolved local edits land on the
//!   remote as soon as the resolver finishes; pull and push touch
//!   disjoint `(kind, slug)` sets (classifier classes are mutually
//!   exclusive) so the order swap doesn't create races.
//! - **Pull-side (`RemoteEdit`, `RemoteCreate`)** — grouped by kind and
//!   handed off to the per-kind pull driver with a `(kind, slug)` subset
//!   filter.
//!
//! Spec: docs/superpowers/specs/2026-05-14-unified-sync-design.md.

use crate::cli::pull::common::{PullCtx, RemoteCatalog, maybe_strip_overlay};
use crate::cli::resolve::{PullAborted, Resolution, prompt_remote_delete, prompt_resolve};
use crate::cli::stdin_coord::CoordinatorStdin;
use crate::cli::sync::classify::{ClassifiedItem, SyncClass};
use crate::log::{Action, Log};
use crate::slug::slugify_unique;
use crate::snapshot::codec::combined_hash;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `(resolution, code_conflict_only, local_bytes, remote_bytes, path)` — the
/// resolved prompt outcome stored in the per-item `RefCell`.
type PromptOutcome = (Resolution, bool, Vec<u8>, Vec<u8>, PathBuf);

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
    progress: &Arc<Log>,
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

    // Build slug → object indexes for the kinds the resolver knows
    // about. Each kind mirrors the slug derivation rule its pull driver
    // uses. The maps stay scoped to this function; downstream resolution
    // looks objects up by slug.
    let mut workspace_by_slug: BTreeMap<String, &crate::model::Workspace> = BTreeMap::new();
    // Side-map of `workspace_url → slug` populated alongside
    // `workspace_by_slug`. Used by the queue loop below as a fallback
    // when `ctx.lockfile.slug_for_url("workspaces", …)` misses — which
    // it does on `doctor --rebuild-lock` (lockfile starts empty) and any
    // resume path where queues were created on remote in a prior failed
    // run but the lockfile was never persisted. Without this, every
    // queue silently `continue`s and the conflict resolver reports
    // "no matching remote object found" for the queue / its schema /
    // its inbox even though the freshly-pulled catalog has them all.
    let mut ws_url_to_slug: BTreeMap<String, String> = BTreeMap::new();
    {
        let mut used: HashSet<String> = HashSet::new();
        for w in &catalog.workspaces {
            let slug = match ctx.lockfile.slug_for_id("workspaces", w.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&w.name, &used),
            };
            used.insert(slug.clone());
            ws_url_to_slug.insert(w.url.clone(), slug.clone());
            workspace_by_slug.insert(slug, w);
        }
    }
    let mut engine_by_slug: BTreeMap<String, &crate::model::Engine> = BTreeMap::new();
    {
        let mut used: HashSet<String> = HashSet::new();
        for e in &catalog.engines {
            let slug = match ctx.lockfile.slug_for_id("engines", e.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&e.name, &used),
            };
            used.insert(slug.clone());
            engine_by_slug.insert(slug, e);
        }
    }
    // Engine fields are nested under their parent engine — the lockfile
    // key is the composite `<engine_slug>/<field_slug>` so two engines
    // can both carry a field named e.g. `Amount` and keep clean
    // per-engine slugs. `slug_for_id` returns a key that may be either
    // composite (post-migration) or legacy flat (older lockfile); both
    // shapes index the catalog by the same composite key here.
    let mut engine_field_by_slug: BTreeMap<String, &crate::model::EngineField> = BTreeMap::new();
    {
        let mut per_engine_used: std::collections::HashMap<String, HashSet<String>> =
            std::collections::HashMap::new();
        for f in &catalog.engine_fields {
            let Some(engine_slug) = ctx
                .lockfile
                .slug_for_url("engines", &f.engine)
                .map(|s| s.to_string())
            else {
                continue;
            };
            let used = per_engine_used.entry(engine_slug.clone()).or_default();
            let field_slug = match ctx.lockfile.slug_for_id("engine_fields", f.id) {
                Some(existing) => existing
                    .strip_prefix(&format!("{engine_slug}/"))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| existing.to_string()),
                None => slugify_unique(&f.name, used),
            };
            used.insert(field_slug.clone());
            engine_field_by_slug.insert(format!("{engine_slug}/{field_slug}"), f);
        }
    }
    let mut hook_by_slug: BTreeMap<String, &crate::model::Hook> = BTreeMap::new();
    {
        let mut used: HashSet<String> = HashSet::new();
        for h in &catalog.hooks {
            let slug = match ctx.lockfile.slug_for_id("hooks", h.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&h.name, &used),
            };
            used.insert(slug.clone());
            hook_by_slug.insert(slug, h);
        }
    }
    let mut rule_by_slug: BTreeMap<String, &crate::model::Rule> = BTreeMap::new();
    {
        let mut used: HashSet<String> = HashSet::new();
        for r in &catalog.rules {
            let slug = match ctx.lockfile.slug_for_id("rules", r.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&r.name, &used),
            };
            used.insert(slug.clone());
            rule_by_slug.insert(slug, r);
        }
    }

    // Queue-tree slug indexes. The queue slug keys queue / schema /
    // inbox; email templates use the compound `<ws>/<q>/<tpl>` slug.
    // Slug derivation mirrors `pull::queues::process` and
    // `pull::email_templates::process`.
    let mut queue_by_slug: BTreeMap<String, (&crate::model::Queue, String /* ws_slug */)> =
        BTreeMap::new();
    let mut q_url_to_ws_q: BTreeMap<String, (String, String)> = BTreeMap::new();
    {
        // Global, id-pinned queue slugs (mirror pull::queues::process): dedup
        // globally, pre-seeded with the already-pinned slugs.
        let mut used_q: HashSet<String> = ctx
            .lockfile
            .objects
            .get("queues")
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        for q in &catalog.queues {
            let Some(ws_url) = q.workspace.as_ref() else {
                continue;
            };
            // Prefer the lockfile slug (preserves user-customised slugs);
            // fall back to the freshly-derived slug from `ws_url_to_slug`
            // when the lockfile has no entry yet. Without the fallback,
            // an empty/freshly-rebuilt lockfile loses every queue here.
            let ws_slug = ctx
                .lockfile
                .slug_for_url("workspaces", ws_url)
                .map(|s| s.to_string())
                .or_else(|| ws_url_to_slug.get(ws_url).cloned());
            let Some(ws_slug) = ws_slug else { continue };
            let q_slug = match ctx.lockfile.slug_for_id("queues", q.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&q.name, &used_q),
            };
            used_q.insert(q_slug.clone());
            q_url_to_ws_q.insert(q.url.clone(), (ws_slug.clone(), q_slug.clone()));
            queue_by_slug.insert(q_slug, (q, ws_slug));
        }
    }
    let email_template_by_compound: BTreeMap<String, &crate::model::EmailTemplate> = {
        let mut map: BTreeMap<String, &crate::model::EmailTemplate> = BTreeMap::new();
        let mut per_q: std::collections::HashMap<(String, String), HashSet<String>> =
            std::collections::HashMap::new();
        for t in &catalog.email_templates {
            let Some(queue_url) = t.queue.as_ref() else {
                continue;
            };
            let Some((ws_slug, q_slug)) = q_url_to_ws_q.get(queue_url).cloned() else {
                continue;
            };
            let used = per_q.entry((ws_slug.clone(), q_slug.clone())).or_default();
            let template_slug = match ctx.lockfile.slug_for_id("email_templates", t.id) {
                Some(existing) => existing.rsplit('/').next().unwrap_or(existing).to_string(),
                None => slugify_unique(&t.name, used),
            };
            used.insert(template_slug.clone());
            map.insert(format!("{ws_slug}/{q_slug}/{template_slug}"), t);
        }
        map
    };

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
        // Resolve the conflict's "remote object" → (remote bytes, local
        // path, id, url, modified_at). Each kind's catalog lookup mirrors
        // its pull driver. mdh is pull-only and never raises BothDiverged
        // here (no push side); we still emit a warning if it shows up.
        let Some(refs) = (match it.kind.as_str() {
            "labels" => label_by_slug.get(it.slug.as_str()).copied().and_then(|l| {
                let codec = crate::snapshot::codec::codec("labels")?;
                let value = serde_json::to_value(l).ok()?;
                let art = codec.disk_bytes(&value).ok()?;
                let ovl_paths = ctx
                    .overlay
                    .as_ref()
                    .and_then(|o| codec.overlay(o, &it.slug));
                let json_bytes = maybe_strip_overlay(art.json, ovl_paths).ok()?;
                let local_path = ctx.paths.labels_dir().join(format!("{}.json", it.slug));
                Some(ConflictRefs {
                    remote_bytes: json_bytes,
                    remote_code: None,
                    remote_formulas: Vec::new(),
                    local_path,
                    id: l.id,
                    modified_at: l.modified_at().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Flat,
                })
            }),
            "workspaces" => workspace_by_slug
                .get(it.slug.as_str())
                .copied()
                .and_then(|w| {
                    let codec = crate::snapshot::codec::codec("workspaces")?;
                    let value = serde_json::to_value(w).ok()?;
                    let art = codec.disk_bytes(&value).ok()?;
                    let ovl_paths = ctx
                        .overlay
                        .as_ref()
                        .and_then(|o| codec.overlay(o, &it.slug));
                    let json_bytes = maybe_strip_overlay(art.json, ovl_paths).ok()?;
                    let local_path = ctx.paths.workspace_dir(&it.slug).join("workspace.json");
                    Some(ConflictRefs {
                        remote_bytes: json_bytes,
                        remote_code: None,
                        remote_formulas: Vec::new(),
                        local_path,
                        id: w.id,
                        modified_at: w.modified_at().map(|s| s.to_string()),
                        hash_strategy: HashStrategy::Flat,
                    })
                }),
            "engines" => engine_by_slug.get(it.slug.as_str()).copied().and_then(|e| {
                let codec = crate::snapshot::codec::codec("engines")?;
                let value = serde_json::to_value(e).ok()?;
                let art = codec.disk_bytes(&value).ok()?;
                let ovl_paths = ctx
                    .overlay
                    .as_ref()
                    .and_then(|o| codec.overlay(o, &it.slug));
                let json_bytes = maybe_strip_overlay(art.json, ovl_paths).ok()?;
                let local_path = ctx.paths.engine_dir(&it.slug).join("engine.json");
                Some(ConflictRefs {
                    remote_bytes: json_bytes,
                    remote_code: None,
                    remote_formulas: Vec::new(),
                    local_path,
                    id: e.id,
                    modified_at: e.modified_at().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Flat,
                })
            }),
            "engine_fields" => {
                engine_field_by_slug
                    .get(it.slug.as_str())
                    .copied()
                    .and_then(|f| {
                        // `it.slug` is the composite `<engine_slug>/<field_slug>`;
                        // split it to derive both the directory and filename.
                        let (engine_slug, field_slug) = it
                            .slug
                            .split_once('/')
                            .map(|(a, b)| (a.to_string(), b.to_string()))?;
                        let codec = crate::snapshot::codec::codec("engine_fields")?;
                        let value = serde_json::to_value(f).ok()?;
                        let art = codec.disk_bytes(&value).ok()?;
                        let ovl_paths = ctx
                            .overlay
                            .as_ref()
                            .and_then(|o| codec.overlay(o, &it.slug));
                        let json_bytes = maybe_strip_overlay(art.json, ovl_paths).ok()?;
                        let local_path = ctx
                            .paths
                            .engine_fields_dir(&engine_slug)
                            .join(format!("{field_slug}.json"));
                        Some(ConflictRefs {
                            remote_bytes: json_bytes,
                            remote_code: None,
                            remote_formulas: Vec::new(),
                            local_path,
                            id: f.id,
                            modified_at: f.modified_at().map(|s| s.to_string()),
                            hash_strategy: HashStrategy::Flat,
                        })
                    })
            }
            "hooks" => hook_by_slug.get(it.slug.as_str()).copied().and_then(|h| {
                // Route through the codec to get the canonical on-disk bytes
                // (status-redacted, code extracted) and apply the overlay strip
                // so the remote bytes match what the pull driver writes to disk.
                let codec = crate::snapshot::codec::codec("hooks")?;
                let value = serde_json::to_value(h).ok()?;
                let art = codec.disk_bytes(&value).ok()?;
                let ovl_paths = ctx
                    .overlay
                    .as_ref()
                    .and_then(|o| codec.overlay(o, &it.slug));
                let json_bytes = maybe_strip_overlay(art.json, ovl_paths).ok()?;
                let code = art
                    .sidecars
                    .into_iter()
                    .find(|(k, _)| k == "code")
                    .and_then(|(_, b)| String::from_utf8(b).ok());
                let local_path = ctx.paths.hooks_dir().join(format!("{}.json", it.slug));
                Some(ConflictRefs {
                    remote_bytes: json_bytes,
                    remote_code: code,
                    remote_formulas: Vec::new(),
                    local_path,
                    id: h.id,
                    modified_at: h.modified_at().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Hook,
                })
            }),
            "rules" => rule_by_slug.get(it.slug.as_str()).copied().and_then(|r| {
                let codec = crate::snapshot::codec::codec("rules")?;
                let value = serde_json::to_value(r).ok()?;
                let art = codec.disk_bytes(&value).ok()?;
                let ovl_paths = ctx
                    .overlay
                    .as_ref()
                    .and_then(|o| codec.overlay(o, &it.slug));
                let json_bytes = maybe_strip_overlay(art.json, ovl_paths).ok()?;
                let code = art
                    .sidecars
                    .into_iter()
                    .find(|(k, _)| k == "trigger_condition")
                    .and_then(|(_, b)| String::from_utf8(b).ok());
                let local_path = ctx.paths.rules_dir().join(format!("{}.json", it.slug));
                Some(ConflictRefs {
                    remote_bytes: json_bytes,
                    remote_code: code,
                    remote_formulas: Vec::new(),
                    local_path,
                    id: r.id,
                    modified_at: r.modified_at().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Rule,
                })
            }),
            "queues" => queue_by_slug
                .get(it.slug.as_str())
                .and_then(|(q, ws_slug)| {
                    // Route through codec to get canonical on-disk bytes
                    // (counts-redacted) and apply overlay strip so the remote
                    // bytes match what the pull driver writes to disk.
                    let codec = crate::snapshot::codec::codec("queues")?;
                    let value = serde_json::to_value(*q).ok()?;
                    let art = codec.disk_bytes(&value).ok()?;
                    let ovl_paths = ctx
                        .overlay
                        .as_ref()
                        .and_then(|o| codec.overlay(o, &it.slug));
                    let json_bytes = maybe_strip_overlay(art.json, ovl_paths).ok()?;
                    let local_path = ctx.paths.queue_dir(ws_slug, &it.slug).join("queue.json");
                    Some(ConflictRefs {
                        remote_bytes: json_bytes,
                        remote_code: None,
                        remote_formulas: Vec::new(),
                        local_path,
                        id: q.id,
                        modified_at: q.modified_at().map(|s| s.to_string()),
                        hash_strategy: HashStrategy::Flat,
                    })
                }),
            "schemas" => {
                // Route through codec so we get the canonical json + formula
                // sidecars and apply the overlay strip. The combined_hash over
                // (stripped_json, sidecars) matches what pull::queues records.
                queue_by_slug
                    .get(it.slug.as_str())
                    .and_then(|(q, ws_slug)| {
                        let schema = catalog.schemas_by_queue_id.get(&q.id)?;
                        let codec = crate::snapshot::codec::codec("schemas")?;
                        let value = serde_json::to_value(schema).ok()?;
                        let art = codec.disk_bytes(&value).ok()?;
                        // Schema slug for the codec overlay lookup is the
                        // composite `<ws_slug>/<q_slug>` key.
                        let composite = format!("{ws_slug}/{}", it.slug);
                        let ovl_paths = ctx
                            .overlay
                            .as_ref()
                            .and_then(|o| codec.overlay(o, &composite));
                        let json_bytes = maybe_strip_overlay(art.json, ovl_paths).ok()?;
                        // Sidecars are `"formulas/<field_id>.py"` → extract
                        // the `(field_id, bytes)` pairs for the resolver.
                        let formulas: Vec<(String, Vec<u8>)> = art
                            .sidecars
                            .into_iter()
                            .filter_map(|(path, b)| {
                                let id = path
                                    .strip_prefix("formulas/")
                                    .and_then(|s| s.strip_suffix(".py"))?;
                                Some((id.to_string(), b))
                            })
                            .collect();
                        let local_path = ctx.paths.queue_dir(ws_slug, &it.slug).join("schema.json");
                        Some(ConflictRefs {
                            remote_bytes: json_bytes,
                            remote_code: None,
                            remote_formulas: formulas,
                            local_path,
                            id: schema.id,
                            modified_at: schema.modified_at().map(|s| s.to_string()),
                            hash_strategy: HashStrategy::Schema,
                        })
                    })
            }
            "inboxes" => queue_by_slug
                .get(it.slug.as_str())
                .and_then(|(q, ws_slug)| {
                    let inbox = catalog.inboxes_by_queue_id.get(&q.id)?;
                    let codec = crate::snapshot::codec::codec("inboxes")?;
                    let value = serde_json::to_value(inbox).ok()?;
                    let art = codec.disk_bytes(&value).ok()?;
                    let ovl_paths = ctx
                        .overlay
                        .as_ref()
                        .and_then(|o| codec.overlay(o, &it.slug));
                    let json_bytes = maybe_strip_overlay(art.json, ovl_paths).ok()?;
                    let local_path = ctx.paths.queue_dir(ws_slug, &it.slug).join("inbox.json");
                    Some(ConflictRefs {
                        remote_bytes: json_bytes,
                        remote_code: None,
                        remote_formulas: Vec::new(),
                        local_path,
                        id: inbox.id,
                        modified_at: inbox.modified_at().map(|s| s.to_string()),
                        hash_strategy: HashStrategy::Flat,
                    })
                }),
            "email_templates" => {
                email_template_by_compound
                    .get(it.slug.as_str())
                    .copied()
                    .and_then(|t| {
                        // Compound slug split: `<ws>/<q>/<tpl>`.
                        let parts: Vec<&str> = it.slug.splitn(3, '/').collect();
                        if parts.len() != 3 {
                            return None;
                        }
                        let codec = crate::snapshot::codec::codec("email_templates")?;
                        let value = serde_json::to_value(t).ok()?;
                        let art = codec.disk_bytes(&value).ok()?;
                        let ovl_paths = ctx
                            .overlay
                            .as_ref()
                            .and_then(|o| codec.overlay(o, &it.slug));
                        let json_bytes = maybe_strip_overlay(art.json, ovl_paths).ok()?;
                        let local_path = ctx
                            .paths
                            .queue_email_templates_dir(parts[0], parts[1])
                            .join(format!("{}.json", parts[2]));
                        Some(ConflictRefs {
                            remote_bytes: json_bytes,
                            remote_code: None,
                            remote_formulas: Vec::new(),
                            local_path,
                            id: t.id,
                            modified_at: t.modified_at().map(|s| s.to_string()),
                            hash_strategy: HashStrategy::Flat,
                        })
                    })
            }
            other => {
                progress.event(
                    Action::Warn,
                    &format!(
                        "conflict resolver not wired for kind '{}' (slug '{}'); skipping",
                        other, it.slug,
                    ),
                );
                None
            }
        }) else {
            // No catalog entry / orphan / unwired kind — warn and move on.
            progress.event(
                Action::Warn,
                &format!(
                    "conflict for {}/{} but no matching remote object found; skipping",
                    it.kind, it.slug,
                ),
            );
            continue;
        };

        resolve_one_conflict(
            ctx,
            it,
            refs,
            idx + 1,
            total,
            &mut input,
            &mut stderr_lock,
            interactive,
            &env,
            progress,
            &mut outcome,
        )?;
    }

    Ok(outcome)
}

/// Per-kind references gathered for a single `BothDiverged` resolution:
/// the remote-canonical bytes the prompt needs, the on-disk path the
/// resolution writes back to, and the lockfile fields (id / url /
/// modified_at) the resolver records when it updates the entry.
///
/// For split-file kinds (`hooks`, `rules`), `remote_code` carries the
/// extracted Python sidecar bytes; `hash_strategy` picks the canonical
/// hash function the resolver records into the lockfile. The resolver
/// prompt itself still displays only the JSON portion — the split-file
/// complexity is deferred per the unified-sync spec; users see a
/// JSON-only diff and the `.py` follows the same `[k]` / `[r]` decision.
struct ConflictRefs {
    remote_bytes: Vec<u8>,
    /// Extracted code sidecar (e.g. `config.code` for hooks,
    /// `trigger_condition` for rules) — `None` for flat kinds and for
    /// split-file kinds whose remote happens not to carry code.
    remote_code: Option<String>,
    /// For `HashStrategy::Schema`, the list of `(field_id, formula_bytes)`
    /// sidecars extracted from the remote schema. Empty for other
    /// strategies. Used by the resolver to (a) compute the canonical
    /// `schema_combined_hash` so the lockfile records the right base,
    /// and (b) write/remove formula sidecars on `[r]` (adopt remote).
    remote_formulas: Vec<(String, Vec<u8>)>,
    local_path: PathBuf,
    id: u64,
    modified_at: Option<String>,
    /// How to fold `(remote_bytes, remote_code)` into the lockfile
    /// `content_hash`. Flat kinds use `Flat` (just `content_hash`);
    /// `Hook` / `Rule` use their respective combined-hash helpers so the
    /// adapter sees `Clean` after the resolution. `Schema` includes
    /// the formula sidecar bytes.
    hash_strategy: HashStrategy,
}

/// Selects the canonical hash function for a kind. Mirrors the per-kind
/// rules in `pull::<kind>::process` and the push scanner.
#[derive(Clone, Copy)]
enum HashStrategy {
    /// Single-file kind: `content_hash(remote_bytes)`.
    Flat,
    /// `hooks`: `hook_combined_hash(json_bytes, code)`.
    Hook,
    /// `rules`: `rule_combined_hash(json_bytes, code)`.
    Rule,
    /// `schemas`: `schema_combined_hash(json_bytes, formulas)` —
    /// formulas are a list of `(field_id, bytes)` sidecars that live
    /// under `formulas/<field_id>.py`. The canonical hash mirrors what
    /// `pull::queues::write_schema_for_queue` and the push scanner
    /// record. Without this strategy the resolver would only hash
    /// the JSON portion and a formula-only divergence would
    /// silently round-trip through `KeepLocal`.
    Schema,
}

impl HashStrategy {
    /// Compute the canonical lockfile hash for the resolved bytes.
    ///
    /// Uses `combined_hash` (the codec-compatible algorithm) for all
    /// split-file kinds so the hash matches what the pull drivers record.
    /// For `Flat` kinds the combined_hash with an empty sidecar list equals
    /// `content_hash`, so both are equivalent.
    /// For `Schema`, `code` is unused (the formulas sidecar list is
    /// derived from the queue dir by the caller); use `hash_schema`
    /// instead.
    fn hash(
        self,
        json_bytes: &[u8],
        code: &Option<String>,
        lockfile: &crate::state::Lockfile,
    ) -> String {
        match self {
            HashStrategy::Flat => combined_hash(json_bytes, &[], lockfile),
            HashStrategy::Hook => {
                let sidecars: Vec<(String, Vec<u8>)> = if let Some(c) = code {
                    vec![("code".to_string(), c.as_bytes().to_vec())]
                } else {
                    vec![]
                };
                combined_hash(json_bytes, &sidecars, lockfile)
            }
            HashStrategy::Rule => {
                let sidecars: Vec<(String, Vec<u8>)> = if let Some(c) = code {
                    vec![("trigger_condition".to_string(), c.as_bytes().to_vec())]
                } else {
                    vec![]
                };
                combined_hash(json_bytes, &sidecars, lockfile)
            }
            // For Schema the caller must use `hash_schema` so the
            // formulas sidecar list is included. Falling back to the
            // bare json-only hash here would silently drop the
            // formulas from the canonical hash — exactly the bug
            // this strategy exists to fix.
            HashStrategy::Schema => combined_hash(json_bytes, &[], lockfile),
        }
    }

    /// Compute the canonical lockfile hash for schema items, including
    /// formulas. Use this for `HashStrategy::Schema` instead of `hash`.
    fn hash_schema(
        self,
        json_bytes: &[u8],
        formulas: &[(String, Vec<u8>)],
        lockfile: &crate::state::Lockfile,
    ) -> String {
        debug_assert!(matches!(self, HashStrategy::Schema));
        // Frame each formula as `"formulas/<field_id>.py"` to match the
        // labels used by the schemas codec and `schema_combined_hash`.
        let sidecars: Vec<(String, Vec<u8>)> = formulas
            .iter()
            .map(|(id, b)| (format!("formulas/{id}.py"), b.clone()))
            .collect();
        combined_hash(json_bytes, &sidecars, lockfile)
    }
}

/// Run the conflict resolution loop for one item: prompts (or applies
/// the non-tty fallback) and routes the outcome to writes + lockfile
/// updates + push-side promotions. Extracted so each kind's arm above
/// stays a thin lookup; behavior matches the inlined labels block this
/// replaced.
///
/// Safety contract (defense layer 2 — see module-level docs):
/// For combined-hash kinds (`Hook`, `Rule`, `Schema`) we DON'T trust
/// `prompt_resolve`'s `local_canonical == remote_canonical` short-
/// circuit, because that only compares the JSON portion. If the JSON
/// canonicalizes-equal but the sidecar (`.py` for hooks/rules,
/// `formulas/*.py` for schemas) differs — symmetric or asymmetric —
/// the short-circuit would silently return `KeepLocal`, promoting
/// the item to push and overwriting the divergent remote state.
///
/// Result of a successful 3-way auto-merge:
/// `(combined_hash, local_paths, remote_paths)`. The caller uses
/// `combined_hash` to update the lockfile entry; the path lists feed
/// the one-line log summary.
type AutoMergeResult = (String, Vec<String>, Vec<String>);

/// Per-strategy 3-way merge attempt. Returns `Some(...)` when the
/// merge auto-resolves cleanly and the merged bytes have been written
/// to disk + base cache; `None` means the caller should fall through
/// to the existing interactive / shadow-file flow.
///
/// Strategy dispatch:
/// * `Flat` — single JSON file, `merge3_json` against the on-disk
///   `local_path`. Output written to `local_path` + cache mirror.
/// * `Hook` / `Rule` — `merge3_json` for the JSON file + strict
///   `merge3_sidecar` for the code file (`.py` / `.js`). Both must
///   resolve cleanly.
/// * `Schema` — `merge3_json` for the schema body + strict
///   `merge3_sidecar` for each formula `.py`. All must resolve.
#[allow(clippy::too_many_arguments)]
fn try_auto_merge(
    ctx: &mut PullCtx<'_>,
    kind: &str,
    slug: &str,
    hash_strategy: HashStrategy,
    lockfile_base_hash: &Option<String>,
    local_path: &Path,
    code_path: &Path,
    local_json_bytes: &[u8],
    local_code: &Option<String>,
    local_formulas: &[(String, Vec<u8>)],
    remote_bytes: &[u8],
    remote_code: &Option<String>,
    remote_formulas: &[(String, Vec<u8>)],
) -> Result<Option<AutoMergeResult>> {
    let Some(base_hash) = lockfile_base_hash else {
        return Ok(None);
    };

    // Step 1: Read the JSON base from cache; bail if absent/stale for
    // any reason. Subsequent dispatches reuse this.
    let Ok(Some(base_json_bytes)) = crate::state::base_cache::read(ctx.paths, local_path) else {
        return Ok(None);
    };

    // Helper: strip noise fields a fresh pull would strip from disk
    // (HIDDEN_FIELDS — currently just `modified_at`) so the merge
    // never introduces them into the merged output.
    let parse_and_strip = |bytes: &[u8]| -> Option<serde_json::Value> {
        let mut v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        crate::snapshot::key_order::strip_hidden_fields_recursive(&mut v);
        Some(v)
    };
    let Some(base_v) = parse_and_strip(&base_json_bytes) else {
        return Ok(None);
    };
    let Some(local_v) = parse_and_strip(local_json_bytes) else {
        return Ok(None);
    };
    let Some(remote_v) = parse_and_strip(remote_bytes) else {
        return Ok(None);
    };

    // Helper: canonicalize merged JSON through the codec for this kind
    // (re-applies redact/hidden-strip so a merge can't bake in a live
    // `counts`/`agenda_id`/`status` field) and apply the overlay strip
    // so overlay-managed fields don't get baked into the merged bytes.
    // Returns `(canonical_json, sidecars)`.
    #[allow(clippy::type_complexity)]
    let canonicalize_merged_json =
        |v: serde_json::Value| -> Option<(Vec<u8>, Vec<(String, Vec<u8>)>)> {
            let codec = crate::snapshot::codec::codec(kind)?;
            let art = codec.disk_bytes(&v).ok()?;
            let ovl_paths = ctx.overlay.as_ref().and_then(|o| codec.overlay(o, slug));
            let json_out = maybe_strip_overlay(art.json, ovl_paths).ok()?;
            Some((json_out, art.sidecars))
        };

    match hash_strategy {
        HashStrategy::Flat => {
            // Single-file merge. cache_matches_base is exact: the
            // cached JSON bytes hash to the lockfile combined_hash.
            if &combined_hash(&base_json_bytes, &[], ctx.lockfile) != base_hash {
                return Ok(None);
            }
            let crate::merge::MergeOutcome::Merged {
                merged,
                local_paths,
                remote_paths,
            } = crate::merge::json3::merge3_json(&base_v, &local_v, &remote_v)
            else {
                return Ok(None);
            };
            // Canonicalize through codec + overlay so the merged bytes
            // match what a fresh pull would write for the same remote value.
            let (merged_bytes, _) = match canonicalize_merged_json(merged) {
                Some(pair) => pair,
                None => {
                    // Codec not registered for this kind (shouldn't happen
                    // for flat kinds the executor handles, but be defensive).
                    let mut b = serde_json::to_vec_pretty(
                        &serde_json::from_slice::<serde_json::Value>(local_json_bytes)
                            .unwrap_or(serde_json::Value::Null),
                    )?;
                    b.push(b'\n');
                    (b, vec![])
                }
            };
            crate::snapshot::writer::write_atomic(local_path, &merged_bytes)?;
            crate::state::base_cache::write(ctx.paths, local_path, &merged_bytes)?;
            let merged_hash = combined_hash(&merged_bytes, &[], ctx.lockfile);
            Ok(Some((merged_hash, local_paths, remote_paths)))
        }
        HashStrategy::Hook | HashStrategy::Rule => {
            // JSON + single code sidecar (.py or .js for hooks; .py
            // for rules). The lockfile records the combined hash —
            // we read the cached sidecar (if any) to reconstruct it.
            let base_code_bytes = crate::state::base_cache::read(ctx.paths, code_path)
                .ok()
                .flatten();
            let base_code_str: Option<String> = base_code_bytes
                .as_ref()
                .and_then(|b| String::from_utf8(b.clone()).ok());
            // Compute the cached combined hash using the codec-compatible
            // algorithm (combined_hash with the correct sidecar label).
            let sidecar_label = if matches!(hash_strategy, HashStrategy::Hook) {
                "code"
            } else {
                "trigger_condition"
            };
            let base_sidecars: Vec<(String, Vec<u8>)> = if let Some(c) = &base_code_str {
                vec![(sidecar_label.to_string(), c.as_bytes().to_vec())]
            } else {
                vec![]
            };
            let cached_combined = combined_hash(&base_json_bytes, &base_sidecars, ctx.lockfile);
            if &cached_combined != base_hash {
                return Ok(None);
            }

            let crate::merge::MergeOutcome::Merged {
                merged: json_merged,
                mut local_paths,
                mut remote_paths,
            } = crate::merge::json3::merge3_json(&base_v, &local_v, &remote_v)
            else {
                return Ok(None);
            };
            let code_label = if matches!(hash_strategy, HashStrategy::Hook) {
                "hook.code"
            } else {
                "rule.code"
            };
            let crate::merge::MergeOutcome::Merged {
                merged: code_merged_bytes,
                local_paths: code_lp,
                remote_paths: code_rp,
            } = crate::merge::sidecar::merge3_sidecar(
                base_code_str.as_deref().unwrap_or("").as_bytes(),
                local_code.as_deref().unwrap_or("").as_bytes(),
                remote_code.as_deref().unwrap_or("").as_bytes(),
                code_label,
            )
            else {
                return Ok(None);
            };
            local_paths.extend(code_lp);
            remote_paths.extend(code_rp);

            // Canonicalize merged JSON through the codec + overlay strip.
            let merged_json_bytes = match canonicalize_merged_json(json_merged) {
                Some((bytes, _)) => bytes,
                None => {
                    let mut b = serde_json::to_vec_pretty(
                        &serde_json::from_slice::<serde_json::Value>(local_json_bytes)
                            .unwrap_or(serde_json::Value::Null),
                    )?;
                    b.push(b'\n');
                    b
                }
            };
            crate::snapshot::writer::write_atomic(local_path, &merged_json_bytes)?;
            crate::state::base_cache::write(ctx.paths, local_path, &merged_json_bytes)?;

            // Sidecar: write if non-empty, otherwise delete + forget.
            let merged_code_str = String::from_utf8(code_merged_bytes).ok();
            let final_code: Option<String> = merged_code_str.filter(|s| !s.is_empty());
            if let Some(c) = &final_code {
                crate::snapshot::writer::write_atomic(code_path, c.as_bytes())?;
                crate::state::base_cache::write(ctx.paths, code_path, c.as_bytes())?;
            } else if code_path.exists() {
                std::fs::remove_file(code_path).with_context(|| {
                    format!("removing merged-empty sidecar {}", code_path.display())
                })?;
                crate::state::base_cache::forget(ctx.paths, code_path)?;
            }

            let final_sidecars: Vec<(String, Vec<u8>)> = if let Some(c) = &final_code {
                vec![(sidecar_label.to_string(), c.as_bytes().to_vec())]
            } else {
                vec![]
            };
            let merged_combined = combined_hash(&merged_json_bytes, &final_sidecars, ctx.lockfile);
            Ok(Some((merged_combined, local_paths, remote_paths)))
        }
        HashStrategy::Schema => {
            // Schema body + zero-to-many formula `.py` sidecars under
            // `<queue_dir>/formulas/`. Each formula is strict-merged
            // independently; the JSON body uses Tier-C recursive
            // merge (id-keyed `content[]` benefits).
            let queue_dir = match local_path.parent() {
                Some(d) => d,
                None => return Ok(None),
            };
            // Read base formulas from cache (preserves slug order).
            let cache_formulas_dir =
                crate::state::base_cache::cache_mirror(ctx.paths, &queue_dir.join("formulas"))
                    .unwrap_or_else(|| ctx.paths.base_cache_root().join("__missing__"));
            let mut base_formulas: Vec<(String, Vec<u8>)> = Vec::new();
            if cache_formulas_dir.exists()
                && let Ok(entries) = std::fs::read_dir(&cache_formulas_dir)
            {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if let Some(id) = name.strip_suffix(".py")
                        && let Ok(b) = std::fs::read(e.path())
                    {
                        base_formulas.push((id.to_string(), b));
                    }
                }
                base_formulas.sort_by(|a, b| a.0.cmp(&b.0));
            }
            // Frame as framed sidecars for combined_hash compatibility.
            let base_framed: Vec<(String, Vec<u8>)> = base_formulas
                .iter()
                .map(|(id, b)| (format!("formulas/{id}.py"), b.clone()))
                .collect();
            let cached_combined = combined_hash(&base_json_bytes, &base_framed, ctx.lockfile);
            if &cached_combined != base_hash {
                return Ok(None);
            }

            let crate::merge::MergeOutcome::Merged {
                merged: json_merged,
                mut local_paths,
                mut remote_paths,
            } = crate::merge::json3::merge3_json(&base_v, &local_v, &remote_v)
            else {
                return Ok(None);
            };

            // Per-formula strict merge. Union of ids across all three
            // sides. Empty formula on a side = "absent".
            use std::collections::BTreeSet;
            let mut ids: BTreeSet<String> = BTreeSet::new();
            for (id, _) in &base_formulas {
                ids.insert(id.clone());
            }
            for (id, _) in local_formulas {
                ids.insert(id.clone());
            }
            for (id, _) in remote_formulas {
                ids.insert(id.clone());
            }
            let mut merged_formulas: Vec<(String, Vec<u8>)> = Vec::new();
            for id in &ids {
                let b = base_formulas
                    .iter()
                    .find(|(i, _)| i == id)
                    .map(|(_, v)| v.as_slice())
                    .unwrap_or(&[]);
                let l = local_formulas
                    .iter()
                    .find(|(i, _)| i == id)
                    .map(|(_, v)| v.as_slice())
                    .unwrap_or(&[]);
                let r = remote_formulas
                    .iter()
                    .find(|(i, _)| i == id)
                    .map(|(_, v)| v.as_slice())
                    .unwrap_or(&[]);
                let outcome =
                    crate::merge::sidecar::merge3_sidecar(b, l, r, &format!("schema.formula.{id}"));
                let crate::merge::MergeOutcome::Merged {
                    merged: m,
                    local_paths: lp,
                    remote_paths: rp,
                } = outcome
                else {
                    return Ok(None);
                };
                local_paths.extend(lp);
                remote_paths.extend(rp);
                if !m.is_empty() {
                    merged_formulas.push((id.clone(), m));
                }
            }

            // Canonicalize the merged schema JSON through the codec +
            // overlay strip before writing, so overlay-managed fields
            // don't get baked into the merged output.
            let merged_json_bytes = match canonicalize_merged_json(json_merged) {
                Some((bytes, _)) => bytes,
                None => {
                    let mut b = serde_json::to_vec_pretty(
                        &serde_json::from_slice::<serde_json::Value>(local_json_bytes)
                            .unwrap_or(serde_json::Value::Null),
                    )?;
                    b.push(b'\n');
                    b
                }
            };
            // Write schema.json, formulas/*.py, AND sweep orphan formula
            // sidecars (formulas removed by one side, other side left
            // base alone).
            crate::snapshot::schema::write_schema_bytes_with_cache(
                queue_dir,
                &merged_json_bytes,
                &merged_formulas,
                Some(ctx.paths),
            )?;
            // Hash using framed sidecars for combined_hash compatibility.
            let merged_framed: Vec<(String, Vec<u8>)> = merged_formulas
                .iter()
                .map(|(id, b)| (format!("formulas/{id}.py"), b.clone()))
                .collect();
            let merged_combined = combined_hash(&merged_json_bytes, &merged_framed, ctx.lockfile);
            Ok(Some((merged_combined, local_paths, remote_paths)))
        }
    }
}

/// To prevent that, this function:
/// - Detects sidecar divergence directly (regardless of JSON state).
/// - When sidecar diverges, redirects the prompt to the sidecar bytes
///   via `prompt_resolve_with_bytes` (which doesn't require the path
///   to exist — important for asymmetric "remote has code, local
///   doesn't" cases).
/// - Records the canonical combined hash on every Resolution arm so
///   the lockfile stays consistent with the classifier's view.
#[allow(clippy::too_many_arguments)]
fn resolve_one_conflict<R: BufRead>(
    ctx: &mut PullCtx<'_>,
    it: &ClassifiedItem,
    refs: ConflictRefs,
    idx_one_based: usize,
    total: usize,
    input: &mut R,
    stderr_lock: &mut std::io::StderrLock<'_>,
    interactive: bool,
    env: &str,
    progress: &Arc<Log>,
    outcome: &mut ConflictOutcome,
) -> Result<()> {
    let ConflictRefs {
        remote_bytes,
        remote_code,
        remote_formulas,
        local_path,
        id,
        modified_at,
        hash_strategy,
    } = refs;

    // For split-file kinds, the sidecar lives next to the JSON. For
    // hooks the extension depends on `config.runtime` (`.py` or `.js`);
    // for rules it is always `.py` (Python is the only valid trigger
    // language). For schemas, the sidecars live in `formulas/<id>.py`
    // under the queue dir. The resolver writes the sidecar on `[r]`
    // (adopt remote) and the local hash on `[s]`/non-tty paths
    // includes whatever code currently sits on disk.
    let code_path = sidecar_path_for_conflict(&local_path, hash_strategy);

    // Read the local code (sidecar) state once; it informs both the
    // redirect decision and the canonical-hash recompute on Skip /
    // non-interactive paths. Schemas don't use `code_path` for their
    // sidecars (they have a directory of formulas); instead we
    // re-derive the local formulas list from the queue dir.
    let (local_code, local_formulas): (Option<String>, Vec<(String, Vec<u8>)>) = match hash_strategy
    {
        HashStrategy::Hook | HashStrategy::Rule => {
            let code = if code_path.exists() {
                std::fs::read_to_string(&code_path).ok()
            } else {
                None
            };
            (code, Vec::new())
        }
        HashStrategy::Schema => {
            // Local formulas live under `<queue_dir>/formulas/`.
            // `local_path` is `<queue_dir>/schema.json` so its
            // parent is the queue dir.
            let queue_dir = local_path.parent().unwrap_or(&local_path);
            let formulas =
                crate::snapshot::schema::read_local_formulas(queue_dir).unwrap_or_default();
            (None, formulas)
        }
        // MDH never reaches the BothDiverged conflict resolver
        // (classifier marks it pull-only), but the match must be
        // exhaustive — treat as a flat single-file kind.
        HashStrategy::Flat => (None, Vec::new()),
    };

    // Decide whether the divergence is in the JSON portion or only in
    // the sidecar. `sidecar_diverges` covers both symmetric (both
    // sides have differing code) AND asymmetric cases (one side has
    // code, the other doesn't). Without this branch firing on the
    // asymmetric cases, the resolver hands `prompt_resolve` a
    // canonicalize-equal JSON pair which short-circuits to
    // `KeepLocal` — silently routing the item to push.
    let local_json_bytes = std::fs::read(&local_path).unwrap_or_default();
    // Use the env lockfile (not an empty one) so reference normalization is
    // applied symmetrically: the on-disk local is in rdc:// form while the
    // remote is in URL form, and only `ctx.lockfile` lets them canonicalize
    // equal. Without it a code-only divergence is mis-read as a JSON divergence
    // and the shadow lands next to the .json instead of the .py.
    let local_json_canon =
        crate::snapshot::noise::canonicalize_for_hash(&local_json_bytes, ctx.lockfile);
    let remote_json_canon =
        crate::snapshot::noise::canonicalize_for_hash(&remote_bytes, ctx.lockfile);
    let json_canonicalize_equal = local_json_canon == remote_json_canon;
    let sidecar_diverges = match hash_strategy {
        HashStrategy::Hook | HashStrategy::Rule => local_code.as_deref() != remote_code.as_deref(),
        HashStrategy::Schema => {
            // Compare local formulas (slug-sorted by `read_local_formulas`)
            // to remote formulas (slug-sorted by `serialize_schema`).
            local_formulas != remote_formulas
        }
        HashStrategy::Flat => false,
    };

    // Compose the canonical "local combined" bytes used by the
    // bytes-driven prompt and the hash recompute. The "combined" hash
    // is what the classifier compares; the prompt should reflect the
    // same view of state.
    let canonical_remote_hash = match hash_strategy {
        HashStrategy::Schema => {
            hash_strategy.hash_schema(&remote_bytes, &remote_formulas, ctx.lockfile)
        }
        _ => hash_strategy.hash(&remote_bytes, &remote_code, ctx.lockfile),
    };
    let canonical_local_hash = match hash_strategy {
        HashStrategy::Schema => {
            hash_strategy.hash_schema(&local_json_bytes, &local_formulas, ctx.lockfile)
        }
        HashStrategy::Hook | HashStrategy::Rule => {
            hash_strategy.hash(&local_json_bytes, &local_code, ctx.lockfile)
        }
        HashStrategy::Flat => hash_strategy.hash(&local_json_bytes, &None, ctx.lockfile),
    };

    // Defensive sanity: if the canonical hashes already match, the
    // classifier was confused (e.g. by a transient catalog reorder)
    // OR both sides made the IDENTICAL change. Treat as Clean — record
    // the matching hash, refresh the cache so the next merge has the
    // current base, and don't promote.
    if canonical_local_hash == canonical_remote_hash {
        crate::cli::pull::common::record_object(
            ctx.lockfile,
            &it.kind,
            &it.slug,
            id,
            modified_at,
            Some(canonical_local_hash),
        );
        // Refresh cache from disk so its hash matches the new lockfile.
        // Without this, a subsequent BothDiverged would fail the
        // `cache_matches_base` guard and silently skip auto-merge.
        if matches!(hash_strategy, HashStrategy::Flat)
            && let Ok(on_disk) = std::fs::read(&local_path)
        {
            crate::state::base_cache::write(ctx.paths, &local_path, &on_disk)?;
        }
        return Ok(());
    }

    // Auto-merge attempt. For every kind, we:
    //  1. Confirm the cached base bytes are still consistent with the
    //     lockfile (`cache_matches_base`) — otherwise the base is
    //     stale and merging would mix up which side moved away.
    //  2. Dispatch by `HashStrategy`:
    //     * `Flat` — JSON-only `merge3_json` against the on-disk file.
    //     * `Hook` / `Rule` — JSON `merge3_json` + strict sidecar
    //       `merge3_sidecar` for the code file (`.py` / `.js`).
    //     * `Schema` — JSON `merge3_json` for the schema body + a
    //       strict `merge3_sidecar` per formula sidecar (`.py` under
    //       `formulas/`).
    //  3. On success: write merged bytes to env tree + cache mirror,
    //     update the lockfile combined hash, emit a single log line
    //     listing the disjoint edit sites, and skip the prompt.
    //  4. On any `Conflict` (genuine overlap) or missing-data case,
    //     fall through to the existing interactive / shadow-file
    //     flow.
    if let Some((merged_combined_hash, lp, rp)) = try_auto_merge(
        ctx,
        &it.kind,
        &it.slug,
        hash_strategy,
        &it.base_hash,
        &local_path,
        &code_path,
        &local_json_bytes,
        &local_code,
        &local_formulas,
        &remote_bytes,
        &remote_code,
        &remote_formulas,
    )? {
        let render = |xs: &[String]| -> String {
            if xs.is_empty() {
                "<none>".to_string()
            } else if xs.len() <= 3 {
                xs.join(", ")
            } else {
                format!("{}, +{} more", xs[..3].join(", "), xs.len() - 3)
            }
        };
        progress.event(
            Action::Info,
            &format!(
                "auto-merge {}/{} (local: {}; remote: {})",
                it.kind,
                it.slug,
                render(&lp),
                render(&rp),
            ),
        );
        // The merged bytes (already on disk + base cache) incorporate
        // both sides. If the merge kept ANY local-side change (`lp`
        // non-empty), the remote does NOT yet have it — recording
        // base=merged and returning here would leave local==base but
        // remote≠base, so the NEXT sync classifies the slug as
        // `RemoteEdit` and PULLS the remote, silently reverting the
        // local change (data-loss bug). Instead, promote the merged
        // file to the push pipeline exactly like the keep-local `[k]`
        // outcome: pin the lockfile base to the CURRENT remote hash so
        // the push driver's drift check passes (remote_hash == base),
        // then let the PATCH re-record the post-push base. After that
        // remote == merged == local == base → converged, nothing lost.
        if lp.is_empty() {
            // Purely adopting remote (only remote-side fields moved):
            // no local change to propagate. Record base=merged and
            // return — no push needed.
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                modified_at,
                Some(merged_combined_hash),
            );
        } else {
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                modified_at,
                Some(canonical_remote_hash.clone()),
            );
            outcome
                .promoted_to_push
                .push((it.kind.clone(), it.slug.clone(), local_path));
        }
        return Ok(());
    }

    if !interactive {
        // Non-TTY/--yes: fall back to legacy shadow-file behavior so the
        // run still completes without blocking on stdin. The local file
        // stays as-is and the lockfile is pinned to the prior base —
        // advancing it would let the conflict silently disappear on
        // subsequent runs. When there is no prior base (post-`rdc doctor
        // --rebuild-lock` with diverged sides, or any first-encounter
        // conflict), record `None` for the content_hash so the next sync
        // re-classifies as `BothDiverged` and re-prompts — recording
        // local hash here would make the next sync see a one-sided
        // `RemoteEdit` and silently overwrite the local edit.
        //
        // For combined-hash kinds where the JSON portions are byte-
        // identical (post-canonicalize) but the sidecar diverges,
        // land the shadow next to the sidecar so the user sees the
        // actual divergence — a `.json.<env>` shadow with byte-
        // identical-to-local content would be misleading.
        let (shadow_anchor, shadow_bytes): (PathBuf, Vec<u8>) = if json_canonicalize_equal
            && sidecar_diverges
        {
            match hash_strategy {
                HashStrategy::Hook | HashStrategy::Rule => {
                    let remote_code_bytes = remote_code.clone().unwrap_or_default().into_bytes();
                    (code_path.clone(), remote_code_bytes)
                }
                HashStrategy::Schema => {
                    // Pick the first divergent formula as the
                    // representative shadow anchor; the lockfile
                    // base is preserved so the next sync re-prompts
                    // with proper UX. Schemas with multiple
                    // divergent formulas land the shadow on the
                    // first one for visibility.
                    let queue_dir = local_path.parent().unwrap_or(&local_path);
                    let formulas_dir = queue_dir.join("formulas");

                    remote_formulas
                        .iter()
                        .find(|(id, bytes)| {
                            local_formulas
                                .iter()
                                .find(|(lid, _)| lid == id)
                                .map(|(_, lb)| lb)
                                != Some(bytes)
                        })
                        .or_else(|| {
                            // Local formula present, remote absent:
                            // pick the first that's only in local.
                            local_formulas
                                .iter()
                                .find(|(id, _)| !remote_formulas.iter().any(|(rid, _)| rid == id))
                        })
                        .map(|(fid, bytes)| (formulas_dir.join(format!("{fid}.py")), bytes.clone()))
                        .unwrap_or_else(|| (local_path.clone(), remote_bytes.clone()))
                }
                HashStrategy::Flat => (local_path.clone(), remote_bytes.clone()),
            }
        } else {
            (local_path.clone(), remote_bytes.clone())
        };
        let conflict_path = crate::paths::shadow_path_for(&shadow_anchor, env);
        write_atomic(&conflict_path, &shadow_bytes)?;
        progress.event(Action::Warn, &format!(
            "{} conflict: local preserved, remote at {} (lockfile base preserved; re-run to resolve)",
            shadow_anchor.display(),
            conflict_path.display(),
        ));
        let preserved_hash: Option<String> = it.base_hash.clone();
        crate::cli::pull::common::record_object(
            ctx.lockfile,
            &it.kind,
            &it.slug,
            id,
            modified_at,
            preserved_hash,
        );
        return Ok(());
    }

    // Interactive prompt setup. For combined-hash kinds with a
    // JSON-equal-but-sidecar-divergent state, we redirect the prompt
    // to a bytes-driven variant that doesn't require the sidecar to
    // exist on disk — important for asymmetric cases (one side has
    // a sidecar, the other doesn't).
    //
    // `code_conflict_only` tracks the redirect so write-back branches
    // below know to write to the sidecar (not the JSON).
    // Wrap all prompt reads in `with_prompt` so the grid renderer
    // suspends its draw region for the duration of the stdin read.
    // For the log renderer this is a transparent no-op.
    let prompt_out: std::cell::RefCell<Option<PromptOutcome>> = std::cell::RefCell::new(None);
    progress.with_prompt(|| -> anyhow::Result<_> {
        let computed: PromptOutcome = if json_canonicalize_equal && sidecar_diverges {
            match hash_strategy {
                HashStrategy::Hook | HashStrategy::Rule => {
                    let local_bytes = local_code.clone().unwrap_or_default().into_bytes();
                    let remote_bytes_for_prompt =
                        remote_code.clone().unwrap_or_default().into_bytes();
                    let r = crate::cli::resolve::prompt_resolve_with_bytes(
                        &mut *input,
                        &mut *stderr_lock,
                        idx_one_based,
                        total,
                        &code_path,
                        &local_bytes,
                        &remote_bytes_for_prompt,
                        env,
                    )?;
                    (
                        r,
                        true,
                        local_bytes,
                        remote_bytes_for_prompt,
                        code_path.clone(),
                    )
                }
                HashStrategy::Schema => {
                    // Pick the first divergent formula sidecar for the
                    // prompt focus. The user resolves the formula; we
                    // record the schema_combined_hash on resolution.
                    let queue_dir = local_path.parent().unwrap_or(&local_path);
                    let formulas_dir = queue_dir.join("formulas");
                    let (fid, local_b, remote_b) = {
                        let mut chosen: Option<(String, Vec<u8>, Vec<u8>)> = None;
                        for (rid, rbytes) in &remote_formulas {
                            let lb = local_formulas
                                .iter()
                                .find(|(lid, _)| lid == rid)
                                .map(|(_, b)| b.clone());
                            if lb.as_deref() != Some(rbytes.as_slice()) {
                                chosen =
                                    Some((rid.clone(), lb.unwrap_or_default(), rbytes.clone()));
                                break;
                            }
                        }
                        if chosen.is_none() {
                            // Local-only formula (remote dropped it).
                            for (lid, lbytes) in &local_formulas {
                                if !remote_formulas.iter().any(|(rid, _)| rid == lid) {
                                    chosen = Some((lid.clone(), lbytes.clone(), Vec::new()));
                                    break;
                                }
                            }
                        }
                        chosen.unwrap_or_else(|| ("unknown".to_string(), Vec::new(), Vec::new()))
                    };
                    let formula_path = formulas_dir.join(format!("{fid}.py"));
                    let r = crate::cli::resolve::prompt_resolve_with_bytes(
                        &mut *input,
                        &mut *stderr_lock,
                        idx_one_based,
                        total,
                        &formula_path,
                        &local_b,
                        &remote_b,
                        env,
                    )?;
                    // For schemas we don't write the sidecar from the
                    // resolver — the `[r]` path adopts the whole
                    // schema including all formulas (handled below).
                    // `code_conflict_only` remains false because the
                    // KeepLocal/KeepRemote/Edit branches need to
                    // handle the whole schema, not just one formula.
                    (r, false, local_b, remote_b, formula_path)
                }
                HashStrategy::Flat => unreachable!(),
            }
        } else {
            // Standard JSON-based prompt (path-driven; reads
            // `local_path` for local bytes).
            let r = prompt_resolve(
                &mut *input,
                &mut *stderr_lock,
                idx_one_based,
                total,
                &local_path,
                &remote_bytes,
                env,
            )?;
            (
                r,
                false,
                local_json_bytes.clone(),
                remote_bytes.clone(),
                local_path.clone(),
            )
        };
        *prompt_out.borrow_mut() = Some(computed);
        Ok(())
    })?;
    let (resolution, code_conflict_only, prompt_local_bytes, prompt_remote_bytes, prompt_path) =
        prompt_out
            .into_inner()
            .expect("with_prompt must populate the resolution");

    // Suppress unused-warning for prompt_local_bytes when no Skip arm
    // reads it (the variable carries diagnostic value for future hooks).
    let _ = prompt_local_bytes;

    match resolution {
        Resolution::KeepLocal => {
            // Local wins → must PATCH. The push driver re-reads
            // `local_path` and PATCHes the remote; it also drives its
            // own drift detection which will now see local-vs-remote
            // drift again, but the `[k]eep local` decision means the
            // user accepted force-push.
            //
            // Pre-write a lockfile base that matches the current
            // remote so the push driver's drift check passes
            // (remote_hash == base). The PATCH response updates the
            // lockfile to the post-PATCH canonical form.
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                modified_at,
                Some(canonical_remote_hash.clone()),
            );
            outcome
                .promoted_to_push
                .push((it.kind.clone(), it.slug.clone(), local_path));
        }
        Resolution::KeepRemote => {
            // Ensure parent dirs exist (workspaces / engines / engine_fields
            // live in nested directories that may not have been created).
            if let Some(parent) = local_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            write_atomic(&local_path, &remote_bytes)?;
            // Adopt the remote sidecar(s) too. Hooks/rules write a
            // single `.py`/`.js` sidecar; schemas adopt the full
            // formulas dir.
            if matches!(hash_strategy, HashStrategy::Hook | HashStrategy::Rule) {
                let canonical_code_path = if matches!(hash_strategy, HashStrategy::Hook) {
                    sidecar_path_for_remote(&local_path, &remote_bytes)
                } else {
                    code_path.clone()
                };
                if let Some(code) = &remote_code {
                    write_atomic(&canonical_code_path, code.as_bytes())?;
                } else if canonical_code_path.exists() {
                    std::fs::remove_file(&canonical_code_path).with_context(|| {
                        format!("removing stale {}", canonical_code_path.display())
                    })?;
                }
                // If the previous-runtime sidecar still sits on disk
                // (hook just flipped runtimes), drop it.
                if canonical_code_path != code_path && code_path.exists() {
                    std::fs::remove_file(&code_path)
                        .with_context(|| format!("removing stale {}", code_path.display()))?;
                }
            } else if matches!(hash_strategy, HashStrategy::Schema) {
                // Adopt full remote formulas dir: write every remote
                // formula and remove any local formula not present in
                // remote.
                let queue_dir = local_path.parent().unwrap_or(&local_path);
                let formulas_dir = queue_dir.join("formulas");
                if !remote_formulas.is_empty() {
                    std::fs::create_dir_all(&formulas_dir)
                        .with_context(|| format!("creating {}", formulas_dir.display()))?;
                }
                for (fid, bytes) in &remote_formulas {
                    write_atomic(&formulas_dir.join(format!("{fid}.py")), bytes)?;
                }
                // Sweep formulas that exist locally but not remotely.
                for (lid, _) in &local_formulas {
                    if !remote_formulas.iter().any(|(rid, _)| rid == lid) {
                        let stale = formulas_dir.join(format!("{lid}.py"));
                        if stale.exists() {
                            std::fs::remove_file(&stale)
                                .with_context(|| format!("removing stale {}", stale.display()))?;
                        }
                    }
                }
            }
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                modified_at,
                Some(canonical_remote_hash.clone()),
            );
        }
        Resolution::Edit(edited) => {
            // Fully-resolved edit — write bytes to disk and align base to
            // remote so push drift detection succeeds, then promote the
            // item to the push pipeline. When the prompt was redirected
            // to the code sidecar (`code_conflict_only`), the edited
            // bytes are code, not JSON, and must land on the sidecar
            // path; the JSON file is untouched.
            let edit_target = if code_conflict_only {
                &code_path
            } else {
                &local_path
            };
            if let Some(parent) = edit_target.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            write_atomic(edit_target, &edited)?;
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                modified_at,
                Some(canonical_remote_hash.clone()),
            );
            outcome
                .promoted_to_push
                .push((it.kind.clone(), it.slug.clone(), local_path));
        }
        Resolution::EditWithMarkers(edited) => {
            // Hunk walker with `[s]kipped` hunks: the bytes intentionally
            // retain conflict markers. Writing them is fine, but the
            // lockfile MUST stay pinned to the prior base so the next
            // pull/sync re-classifies the slug as a conflict — otherwise
            // the markers get silently baked in as the new base.
            // No push promotion either (the API would reject markers).
            // When prompt was on the sidecar, edited bytes are code.
            let edit_target = if code_conflict_only {
                &code_path
            } else {
                &local_path
            };
            if let Some(parent) = edit_target.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            write_atomic(edit_target, &edited)?;
            progress.event(Action::Warn, &format!(
                "{} partially resolved (markers retained); lockfile base preserved; re-run to resolve",
                edit_target.display(),
            ));
            // When base is absent (post-`rdc doctor --rebuild-lock`, or any
            // first-encounter conflict), record `None` for the content_hash
            // so the next sync re-classifies as `BothDiverged` and re-prompts
            // — recording local hash here would let the next sync see a
            // one-sided `RemoteEdit` and silently overwrite the local edit.
            let preserved_hash: Option<String> = it.base_hash.clone();
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                modified_at,
                preserved_hash,
            );
        }
        Resolution::Skip => {
            // Shadow-file fallback. Write `<prompt>.<env>` with the
            // remote bytes (the same content the prompt would have
            // shown), keep local on disk, pin the lockfile to the prior
            // base so subsequent runs re-prompt. When the prompt was
            // redirected to the sidecar, the shadow file lands next to
            // the sidecar; this way the user gets a `.py.<env>` shadow
            // showing the remote code, not a redundant copy of an
            // identical-to-local `.json`.
            let conflict_path = crate::paths::shadow_path_for(&prompt_path, env);
            write_atomic(&conflict_path, &prompt_remote_bytes)?;
            progress.event(Action::Warn, &format!(
                "{} conflict: local preserved, remote at {} (lockfile base preserved; re-run to resolve)",
                prompt_path.display(),
                conflict_path.display(),
            ));
            // When base is absent (post-`rdc doctor --rebuild-lock`, or any
            // first-encounter conflict), record `None` for the content_hash
            // so the next sync re-classifies as `BothDiverged` and re-prompts
            // — recording local hash here would let the next sync see a
            // one-sided `RemoteEdit` and silently overwrite the local edit.
            let preserved_hash: Option<String> = it.base_hash.clone();
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                modified_at,
                preserved_hash,
            );
        }
        Resolution::Abort => {
            return Err(anyhow::Error::new(PullAborted));
        }
    }

    Ok(())
}

/// Compute the sidecar path for a conflict's `local_path`, given the
/// hash strategy. For hooks we peek at the on-disk JSON's `config.runtime`
/// to decide `.py` vs `.js`; for rules (and the Flat fallback) we always
/// return `<json>.py`. The lookup never fails — a missing or unparseable
/// local file just falls back to `.py`, so the caller's `.exists()`
/// guard determines whether the sidecar contributes at all.
fn sidecar_path_for_conflict(local_path: &Path, strategy: HashStrategy) -> PathBuf {
    match strategy {
        HashStrategy::Hook => {
            if let Ok(bytes) = std::fs::read(local_path)
                && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes)
            {
                let ext = crate::snapshot::hook::hook_code_extension_from_value(&v);
                return local_path.with_extension(ext);
            }
            local_path.with_extension("py")
        }
        // Rules, Flat, and Mdh — `trigger_condition` is always Python
        // (Rules); Flat/Mdh have no sidecar at all but the function
        // contract returns `<json>.py` as a harmless fallback.
        HashStrategy::Rule | HashStrategy::Flat => local_path.with_extension("py"),
        // Schemas don't have a single sidecar — they have a
        // `formulas/<id>.py` per datapoint with a formula. The
        // resolver picks the specific formula at prompt time; this
        // function's return value isn't used on the schema path.
        // Return the formulas directory as a sentinel so a misuse
        // surfaces (writing to a directory fails).
        HashStrategy::Schema => local_path
            .parent()
            .map(|p| p.join("formulas"))
            .unwrap_or_else(|| local_path.with_extension("py")),
    }
}

/// Compute the canonical sidecar path for a hook conflict using *remote*
/// JSON bytes — used on `[r]` (adopt remote) so the sidecar lands at the
/// extension matching the incoming runtime, regardless of what the
/// local-side runtime declared. Unparseable bytes default to `.py`.
fn sidecar_path_for_remote(local_path: &Path, remote_bytes: &[u8]) -> PathBuf {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(remote_bytes) {
        let ext = crate::snapshot::hook::hook_code_extension_from_value(&v);
        return local_path.with_extension(ext);
    }
    local_path.with_extension("py")
}

/// Compute the `<file>.<env>-deleted` marker path for `local_path`.
/// Mirrors [`crate::paths::shadow_path_for`]'s format with a `-deleted`
/// suffix — matches what [`crate::paths::is_shadow_artifact`] already
/// recognises so snapshot walkers and `_index.md` regeneration skip it
/// just like the conflict-skip shadow.
fn deleted_marker_path(local_path: &Path, env: &str) -> PathBuf {
    let shadow = crate::paths::shadow_path_for(local_path, env);
    let mut s = shadow.into_os_string();
    s.push("-deleted");
    PathBuf::from(s)
}

/// Drop a `(kind, slug)` entry from the lockfile. Used by the
/// remote-delete dispatcher when the user accepts the delete (`[r]`),
/// when both sides converged on deletion (`BothDeleted`), or when the
/// user wants to restore on env via POST (`[k]` — push pipeline treats
/// missing lockfile entry as a `LocalCreate`).
fn drop_lockfile_entry(ctx: &mut PullCtx<'_>, kind: &str, slug: &str) {
    if let Some(map) = ctx.lockfile.objects.get_mut(kind) {
        map.remove(slug);
    }
}

/// Remove an orphaned MDH dataset's on-disk dir + base-cache mirror +
/// lockfile entry. Best-effort on the filesystem (idempotent: missing
/// paths are a no-op); the lockfile drop is the load-bearing part so the
/// dataset stops being re-flagged on the next sync.
fn remove_mdh_dataset(ctx: &mut PullCtx<'_>, slug: &str, indexes_path: &Path) {
    let dataset_dir = ctx.paths.dataset_dir(slug);
    if dataset_dir.exists() {
        std::fs::remove_dir_all(&dataset_dir).ok();
    }
    crate::state::base_cache::forget(ctx.paths, indexes_path).ok();
    // Drop the now-empty base-cache dataset dir too (cosmetic; only
    // succeeds if empty).
    if let Some(mirror) = crate::state::base_cache::cache_mirror(ctx.paths, indexes_path)
        && let Some(mirror_dir) = mirror.parent()
    {
        std::fs::remove_dir(mirror_dir).ok();
    }
    drop_lockfile_entry(ctx, "mdh_indexes", slug);
}

/// Reconcile MDH datasets the remote no longer lists.
///
/// MDH bypasses the classifier (see `from_catalog_scan_lockfile`), so a
/// remotely-deleted collection never produces a `RemoteDelete` and was
/// historically orphaned on disk forever (its `envs/<env>/mdh/<slug>/`
/// dir, base-cache mirror, and `mdh_indexes` lockfile entry all
/// persisted). This closes that gap: any `mdh_indexes` lockfile slug
/// absent from `remote_slugs` is an orphan, reconciled with the same UX
/// as the classifier's remote-delete family
/// ([`crate::cli::resolve::prompt_remote_delete`]'s `[k]/[r]/[s]/[a]`):
///
/// - `[r]` use env (delete local): remove the dataset dir + base-cache
///   mirror, drop the lockfile entry. The dominant case.
/// - `[s]` skip: write a `<indexes.json>.<env>-deleted` marker; the next
///   sync re-presents the choice.
/// - `[k]` keep local: MDH has no remote-restore path (`push_dataset`
///   only edits indexes of an *existing* collection — it can't recreate
///   a deleted one), so this keeps the on-disk files but drops the
///   lockfile entry + base cache so the dataset isn't re-flagged every
///   sync. A warning explains the collection is not recreated on the env.
/// - `[a]` abort: propagate [`PullAborted`].
///
/// `interactive == false` (CI / `--yes`) falls back to `[s]` for every
/// orphan, mirroring [`resolve_remote_deletes`], so a non-tty run never
/// silently destroys local files. A lockfile entry whose on-disk
/// `indexes.json` is already gone is a both-sides-agree deletion: the
/// entry + base cache are dropped silently, no prompt.
///
/// Returns the number of datasets removed from disk.
async fn prune_mdh_orphans<R: BufRead>(
    ctx: &mut PullCtx<'_>,
    remote_slugs: &std::collections::BTreeSet<String>,
    mut input: R,
    interactive: bool,
    progress: &Arc<Log>,
) -> Result<usize> {
    let orphan_slugs: Vec<String> = ctx
        .lockfile
        .objects
        .get("mdh_indexes")
        .map(|m| {
            m.keys()
                .filter(|s| !remote_slugs.contains(s.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if orphan_slugs.is_empty() {
        return Ok(0);
    }

    let env = ctx.paths.env().to_string();
    let mut pruned = 0usize;

    for slug in orphan_slugs {
        let indexes_path = ctx.paths.dataset_dir(&slug).join("indexes.json");

        // Both-sides-agree deletion: lockfile entry but no local file.
        if !indexes_path.exists() {
            remove_mdh_dataset(ctx, &slug, &indexes_path);
            progress.event(
                Action::Delete,
                &format!("mdh/{slug} (deleted on {env}; no local file)"),
            );
            pruned += 1;
            continue;
        }

        // Non-tty: defer with a marker, never silently delete local files.
        if !interactive {
            let marker = deleted_marker_path(&indexes_path, &env);
            write_atomic(&marker, b"")?;
            progress.event(
                Action::Warn,
                &format!(
                    "mdh/{slug}: env deletion deferred (non-tty); marker at {}",
                    marker.display(),
                ),
            );
            continue;
        }

        // Interactive prompt — same shape as the classifier kinds.
        let prompt_res: std::cell::RefCell<Option<Resolution>> = std::cell::RefCell::new(None);
        let indexes_for_prompt = indexes_path.clone();
        progress.with_prompt(|| -> anyhow::Result<()> {
            let r = prompt_remote_delete(
                &mut input,
                std::io::stderr().lock(),
                &indexes_for_prompt,
                &env,
            )?;
            *prompt_res.borrow_mut() = Some(r);
            Ok(())
        })?;
        let resolution = prompt_res.into_inner().expect("with_prompt must populate");

        match resolution {
            Resolution::KeepRemote => {
                remove_mdh_dataset(ctx, &slug, &indexes_path);
                progress.event(Action::Delete, &format!("mdh/{slug}"));
                pruned += 1;
            }
            Resolution::KeepLocal => {
                // No remote-restore path for MDH collections. Keep the
                // local files but stop tracking so the dataset isn't
                // re-flagged every sync.
                crate::state::base_cache::forget(ctx.paths, &indexes_path).ok();
                drop_lockfile_entry(ctx, "mdh_indexes", &slug);
                progress.event(
                    Action::Info,
                    &format!(
                        "mdh/{slug}: kept local files; the collection is NOT recreated on {env} \
                         (MDH collections can't be pushed). Recreate it on {env} and re-sync to \
                         re-establish tracking."
                    ),
                );
            }
            Resolution::Skip => {
                let marker = deleted_marker_path(&indexes_path, &env);
                write_atomic(&marker, b"")?;
                progress.event(
                    Action::Warn,
                    &format!("mdh/{slug}: env deletion deferred; marker at {}", marker.display()),
                );
            }
            Resolution::Edit(_) | Resolution::EditWithMarkers(_) | Resolution::Abort => {
                return Err(anyhow::Error::new(PullAborted));
            }
        }
    }

    Ok(pruned)
}

/// Resolve `RemoteDelete`, `LocalEditRemoteDelete`,
/// `LocalDeleteRemoteEdit`, and `BothDeleted` items in `classified`.
///
/// All three "destructive direction" classes share the same prompt
/// shape ([`crate::cli::resolve::prompt_remote_delete`]) and resolve
/// to one of:
/// - `[k]` **keep local** — restore on env. The lockfile entry is
///   dropped and the item is promoted to the push pipeline, which sees
///   the missing lockfile entry and POSTs (see
///   `cli::push::labels::push`'s "missing lockfile entry → new label"
///   branch).
/// - `[r]` **use env** — mirror the env's deletion locally. The local
///   file is removed and the lockfile entry is dropped.
/// - `[s]` **skip / shadow marker** — write a sibling
///   `<file>.<env>-deleted` marker, leave the local file and lockfile
///   alone. The next sync re-presents the same conflict.
/// - `[a]` **abort** — return [`PullAborted`] so the outer driver
///   suppresses `lockfile.save()`.
///
/// `BothDeleted` short-circuits: no prompt, no marker — both sides
/// already agree on deletion so the executor just drops the lockfile
/// entry. Re-running the sync sees `Clean` state.
///
/// `interactive == false` (non-TTY / `--yes`) falls back to `[s]` for
/// every prompted class. For `LocalDeleteRemoteEdit` the env-side
/// bytes are restored to `local_path` before the marker is written
/// so the user has something to review on the next interactive run.
///
/// Only the `labels` kind is wired today (Task 17 scope); other kinds
/// are emitted as warnings and skipped. Subsequent tasks plumb in
/// hashing for the remaining kinds.
pub(crate) async fn resolve_remote_deletes<R: BufRead>(
    ctx: &mut PullCtx<'_>,
    catalog: &RemoteCatalog,
    classified: &[ClassifiedItem],
    mut input: R,
    interactive: bool,
    progress: &Arc<Log>,
) -> Result<ConflictOutcome> {
    let mut outcome = ConflictOutcome::default();

    // Build slug → object indexes for each kind that may surface here.
    // LocalDeleteRemoteEdit needs the env-side body to restore the file
    // for review; KeepRemote on the same class then aligns the lockfile
    // to the restored bytes' hash. Slug derivation mirrors the pull
    // drivers (and `resolve_conflicts`).
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
    let mut workspace_by_slug: BTreeMap<String, &crate::model::Workspace> = BTreeMap::new();
    // Same `workspace_url → slug` side-map as the resolve_conflicts
    // path, used below to map queue.workspace URLs to slugs when the
    // lockfile is empty (rebuild-lock or partial-deploy resume).
    let mut ws_url_to_slug: BTreeMap<String, String> = BTreeMap::new();
    {
        let mut used: HashSet<String> = HashSet::new();
        for w in &catalog.workspaces {
            let slug = match ctx.lockfile.slug_for_id("workspaces", w.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&w.name, &used),
            };
            used.insert(slug.clone());
            ws_url_to_slug.insert(w.url.clone(), slug.clone());
            workspace_by_slug.insert(slug, w);
        }
    }
    let mut engine_by_slug: BTreeMap<String, &crate::model::Engine> = BTreeMap::new();
    {
        let mut used: HashSet<String> = HashSet::new();
        for e in &catalog.engines {
            let slug = match ctx.lockfile.slug_for_id("engines", e.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&e.name, &used),
            };
            used.insert(slug.clone());
            engine_by_slug.insert(slug, e);
        }
    }
    // Engine fields are nested under their parent engine — the lockfile
    // key is the composite `<engine_slug>/<field_slug>` so two engines
    // can both carry a field named e.g. `Amount` and keep clean
    // per-engine slugs. `slug_for_id` returns a key that may be either
    // composite (post-migration) or legacy flat (older lockfile); both
    // shapes index the catalog by the same composite key here.
    let mut engine_field_by_slug: BTreeMap<String, &crate::model::EngineField> = BTreeMap::new();
    {
        let mut per_engine_used: std::collections::HashMap<String, HashSet<String>> =
            std::collections::HashMap::new();
        for f in &catalog.engine_fields {
            let Some(engine_slug) = ctx
                .lockfile
                .slug_for_url("engines", &f.engine)
                .map(|s| s.to_string())
            else {
                continue;
            };
            let used = per_engine_used.entry(engine_slug.clone()).or_default();
            let field_slug = match ctx.lockfile.slug_for_id("engine_fields", f.id) {
                Some(existing) => existing
                    .strip_prefix(&format!("{engine_slug}/"))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| existing.to_string()),
                None => slugify_unique(&f.name, used),
            };
            used.insert(field_slug.clone());
            engine_field_by_slug.insert(format!("{engine_slug}/{field_slug}"), f);
        }
    }
    let mut hook_by_slug: BTreeMap<String, &crate::model::Hook> = BTreeMap::new();
    {
        let mut used: HashSet<String> = HashSet::new();
        for h in &catalog.hooks {
            let slug = match ctx.lockfile.slug_for_id("hooks", h.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&h.name, &used),
            };
            used.insert(slug.clone());
            hook_by_slug.insert(slug, h);
        }
    }
    let mut rule_by_slug: BTreeMap<String, &crate::model::Rule> = BTreeMap::new();
    {
        let mut used: HashSet<String> = HashSet::new();
        for r in &catalog.rules {
            let slug = match ctx.lockfile.slug_for_id("rules", r.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&r.name, &used),
            };
            used.insert(slug.clone());
            rule_by_slug.insert(slug, r);
        }
    }

    // Queue-tree indexes for remote-delete dispatch. Mirrors
    // `resolve_conflicts`. The same compound-slug map for email
    // templates is built here so `<ws>/<q>/<tpl>` → template lookups
    // share the slug derivation.
    let mut queue_by_slug: BTreeMap<String, (&crate::model::Queue, String /* ws_slug */)> =
        BTreeMap::new();
    let mut q_url_to_ws_q: BTreeMap<String, (String, String)> = BTreeMap::new();
    {
        // Global, id-pinned queue slugs (mirror pull::queues::process): dedup
        // globally, pre-seeded with the already-pinned slugs.
        let mut used_q: HashSet<String> = ctx
            .lockfile
            .objects
            .get("queues")
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        for q in &catalog.queues {
            let Some(ws_url) = q.workspace.as_ref() else {
                continue;
            };
            let ws_slug = ctx
                .lockfile
                .slug_for_url("workspaces", ws_url)
                .map(|s| s.to_string())
                .or_else(|| ws_url_to_slug.get(ws_url).cloned());
            let Some(ws_slug) = ws_slug else { continue };
            let q_slug = match ctx.lockfile.slug_for_id("queues", q.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&q.name, &used_q),
            };
            used_q.insert(q_slug.clone());
            q_url_to_ws_q.insert(q.url.clone(), (ws_slug.clone(), q_slug.clone()));
            queue_by_slug.insert(q_slug, (q, ws_slug));
        }
    }
    let email_template_by_compound: BTreeMap<String, &crate::model::EmailTemplate> = {
        let mut map: BTreeMap<String, &crate::model::EmailTemplate> = BTreeMap::new();
        let mut per_q: std::collections::HashMap<(String, String), HashSet<String>> =
            std::collections::HashMap::new();
        for t in &catalog.email_templates {
            let Some(queue_url) = t.queue.as_ref() else {
                continue;
            };
            let Some((ws_slug, q_slug)) = q_url_to_ws_q.get(queue_url).cloned() else {
                continue;
            };
            let used = per_q.entry((ws_slug.clone(), q_slug.clone())).or_default();
            let template_slug = match ctx.lockfile.slug_for_id("email_templates", t.id) {
                Some(existing) => existing.rsplit('/').next().unwrap_or(existing).to_string(),
                None => slugify_unique(&t.name, used),
            };
            used.insert(template_slug.clone());
            map.insert(format!("{ws_slug}/{q_slug}/{template_slug}"), t);
        }
        map
    };

    let env = ctx.paths.env().to_string();

    for it in classified {
        match &it.class {
            SyncClass::BothDeleted => {
                // Silent convergence — both sides removed the object, so
                // the lockfile entry is the only thing left. Drop it.
                // All push-capable kinds use the same drop semantics.
                if matches!(
                    it.kind.as_str(),
                    "labels"
                        | "workspaces"
                        | "engines"
                        | "engine_fields"
                        | "hooks"
                        | "rules"
                        | "queues"
                        | "schemas"
                        | "inboxes"
                        | "email_templates"
                ) {
                    drop_lockfile_entry(ctx, &it.kind, &it.slug);
                } else {
                    progress.event(
                        Action::Warn,
                        &format!(
                            "BothDeleted handler not yet wired for kind '{}' (slug '{}'); skipping",
                            it.kind, it.slug,
                        ),
                    );
                }
                continue;
            }
            SyncClass::RemoteDelete
            | SyncClass::LocalEditRemoteDelete
            | SyncClass::LocalDeleteRemoteEdit => {
                // Compute the per-kind (local_path, restore_bytes,
                // id/url/modified_at) refs. `restore_bytes` is `Some`
                // only when the catalog carries the env-side body
                // (RemoteDelete / LocalEditRemoteDelete may lack it
                // because the env already dropped the object — those
                // classes only need `local_path`).
                let refs_opt: Option<RemoteDeleteRefs> = match it.kind.as_str() {
                    "labels" => {
                        let local_path = ctx.paths.labels_dir().join(format!("{}.json", it.slug));
                        let body = label_by_slug.get(it.slug.as_str()).copied();
                        let restore = body.and_then(|l| {
                            let codec = crate::snapshot::codec::codec("labels")?;
                            let value = serde_json::to_value(l).ok()?;
                            let art = codec.disk_bytes(&value).ok()?;
                            let ovl = ctx
                                .overlay
                                .as_ref()
                                .and_then(|o| codec.overlay(o, &it.slug));
                            let bytes = maybe_strip_overlay(art.json, ovl).ok()?;
                            Some(bytes)
                        });
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: restore,
                            restore_code: None,
                            restore_formulas: Vec::new(),
                            id: body.map(|l| l.id),
                            modified_at: body.and_then(|l| l.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    "workspaces" => {
                        let local_path = ctx.paths.workspace_dir(&it.slug).join("workspace.json");
                        let body = workspace_by_slug.get(it.slug.as_str()).copied();
                        let restore = body.and_then(|w| {
                            let codec = crate::snapshot::codec::codec("workspaces")?;
                            let value = serde_json::to_value(w).ok()?;
                            let art = codec.disk_bytes(&value).ok()?;
                            let ovl = ctx
                                .overlay
                                .as_ref()
                                .and_then(|o| codec.overlay(o, &it.slug));
                            let bytes = maybe_strip_overlay(art.json, ovl).ok()?;
                            Some(bytes)
                        });
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: restore,
                            restore_code: None,
                            restore_formulas: Vec::new(),
                            id: body.map(|w| w.id),
                            modified_at: body.and_then(|w| w.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    "engines" => {
                        let local_path = ctx.paths.engine_dir(&it.slug).join("engine.json");
                        let body = engine_by_slug.get(it.slug.as_str()).copied();
                        let restore = body.and_then(|e| {
                            let codec = crate::snapshot::codec::codec("engines")?;
                            let value = serde_json::to_value(e).ok()?;
                            let art = codec.disk_bytes(&value).ok()?;
                            let ovl = ctx
                                .overlay
                                .as_ref()
                                .and_then(|o| codec.overlay(o, &it.slug));
                            let bytes = maybe_strip_overlay(art.json, ovl).ok()?;
                            Some(bytes)
                        });
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: restore,
                            restore_code: None,
                            restore_formulas: Vec::new(),
                            id: body.map(|e| e.id),
                            modified_at: body.and_then(|e| e.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    "engine_fields" => {
                        let body = engine_field_by_slug.get(it.slug.as_str()).copied();
                        // `it.slug` is composite `<engine_slug>/<field_slug>`.
                        // Split it to derive both the parent dir and the
                        // file name; fall back to a sentinel `__orphan__`
                        // path when the composite shape is unexpected.
                        let (engine_slug_opt, field_slug) = match it.slug.split_once('/') {
                            Some((e, f)) => (Some(e.to_string()), f.to_string()),
                            None => (
                                find_engine_field_engine_slug(ctx.paths, &it.slug),
                                it.slug.clone(),
                            ),
                        };
                        let local_path = match engine_slug_opt {
                            Some(es) => ctx
                                .paths
                                .engine_fields_dir(&es)
                                .join(format!("{field_slug}.json")),
                            None => ctx
                                .paths
                                .engines_dir()
                                .join("__orphan__/fields")
                                .join(format!("{field_slug}.json")),
                        };
                        let restore = body.and_then(|f| {
                            let codec = crate::snapshot::codec::codec("engine_fields")?;
                            let value = serde_json::to_value(f).ok()?;
                            let art = codec.disk_bytes(&value).ok()?;
                            let ovl = ctx
                                .overlay
                                .as_ref()
                                .and_then(|o| codec.overlay(o, &it.slug));
                            let bytes = maybe_strip_overlay(art.json, ovl).ok()?;
                            Some(bytes)
                        });
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: restore,
                            restore_code: None,
                            restore_formulas: Vec::new(),
                            id: body.map(|f| f.id),
                            modified_at: body.and_then(|f| f.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    "hooks" => {
                        let local_path = ctx.paths.hooks_dir().join(format!("{}.json", it.slug));
                        let body = hook_by_slug.get(it.slug.as_str()).copied();
                        // Route through the codec to get the canonical on-disk
                        // bytes (status-redacted, code extracted) and apply the
                        // overlay strip so the restore matches the pull driver.
                        let (restore_bytes, restore_code) = match body.and_then(|h| {
                            let codec = crate::snapshot::codec::codec("hooks")?;
                            let value = serde_json::to_value(h).ok()?;
                            let art = codec.disk_bytes(&value).ok()?;
                            let ovl = ctx
                                .overlay
                                .as_ref()
                                .and_then(|o| codec.overlay(o, &it.slug));
                            let json = maybe_strip_overlay(art.json, ovl).ok()?;
                            let code = art
                                .sidecars
                                .into_iter()
                                .find(|(k, _)| k == "code")
                                .and_then(|(_, b)| String::from_utf8(b).ok());
                            Some((json, code))
                        }) {
                            Some((j, c)) => (Some(j), c),
                            None => (None, None),
                        };
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes,
                            restore_code,
                            restore_formulas: Vec::new(),
                            id: body.map(|h| h.id),
                            modified_at: body.and_then(|h| h.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Hook,
                        })
                    }
                    "rules" => {
                        let local_path = ctx.paths.rules_dir().join(format!("{}.json", it.slug));
                        let body = rule_by_slug.get(it.slug.as_str()).copied();
                        let (restore_bytes, restore_code) = match body.and_then(|r| {
                            let codec = crate::snapshot::codec::codec("rules")?;
                            let value = serde_json::to_value(r).ok()?;
                            let art = codec.disk_bytes(&value).ok()?;
                            let ovl = ctx
                                .overlay
                                .as_ref()
                                .and_then(|o| codec.overlay(o, &it.slug));
                            let json = maybe_strip_overlay(art.json, ovl).ok()?;
                            let code = art
                                .sidecars
                                .into_iter()
                                .find(|(k, _)| k == "trigger_condition")
                                .and_then(|(_, b)| String::from_utf8(b).ok());
                            Some((json, code))
                        }) {
                            Some((j, c)) => (Some(j), c),
                            None => (None, None),
                        };
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes,
                            restore_code,
                            restore_formulas: Vec::new(),
                            id: body.map(|r| r.id),
                            modified_at: body.and_then(|r| r.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Rule,
                        })
                    }
                    "queues" => {
                        let body = queue_by_slug.get(it.slug.as_str());
                        // local_path needs the (ws, q) lookup; for env-side-
                        // dropped queues the catalog body is gone, fall back
                        // to the disk sweep used by `change_list_from_classified`.
                        let local_path = match body {
                            Some((_, ws_slug)) => {
                                ctx.paths.queue_dir(ws_slug, &it.slug).join("queue.json")
                            }
                            None => crate::cli::push::scan::find_queue_nested_path(
                                ctx.paths,
                                &it.slug,
                                "queue.json",
                            )
                            .unwrap_or_else(|| {
                                ctx.paths
                                    .workspaces_dir()
                                    .join("__orphan__/queues")
                                    .join(&it.slug)
                                    .join("queue.json")
                            }),
                        };
                        let restore = body.and_then(|(q, _)| {
                            let codec = crate::snapshot::codec::codec("queues")?;
                            let value = serde_json::to_value(*q).ok()?;
                            let art = codec.disk_bytes(&value).ok()?;
                            let ovl = ctx
                                .overlay
                                .as_ref()
                                .and_then(|o| codec.overlay(o, &it.slug));
                            let bytes = maybe_strip_overlay(art.json, ovl).ok()?;
                            Some(bytes)
                        });
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: restore,
                            restore_code: None,
                            restore_formulas: Vec::new(),
                            id: body.map(|(q, _)| q.id),
                            modified_at: body
                                .and_then(|(q, _)| q.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    "schemas" => {
                        // schemas live alongside the queue at schema.json.
                        // Bug e fix: route through the codec so formula sidecars
                        // are preserved and restored on [r]/[k]. The hash must
                        // be combined_hash(post-overlay json, framed sidecars).
                        let body_pair =
                            queue_by_slug
                                .get(it.slug.as_str())
                                .and_then(|(q, ws_slug)| {
                                    catalog
                                        .schemas_by_queue_id
                                        .get(&q.id)
                                        .map(|s| (s, ws_slug.clone()))
                                });
                        let local_path = match &body_pair {
                            Some((_, ws_slug)) => {
                                ctx.paths.queue_dir(ws_slug, &it.slug).join("schema.json")
                            }
                            None => crate::cli::push::scan::find_queue_nested_path(
                                ctx.paths,
                                &it.slug,
                                "schema.json",
                            )
                            .unwrap_or_else(|| {
                                ctx.paths
                                    .workspaces_dir()
                                    .join("__orphan__/queues")
                                    .join(&it.slug)
                                    .join("schema.json")
                            }),
                        };
                        // Derive codec artifacts (json + formula sidecars) and
                        // apply overlay strip. The composite slug for overlay
                        // lookup is `<ws_slug>/<q_slug>`.
                        let (restore_bytes, restore_formulas) =
                            match body_pair.as_ref().and_then(|(s, ws_slug)| {
                                let composite = format!("{ws_slug}/{}", it.slug);
                                let codec = crate::snapshot::codec::codec("schemas")?;
                                let value = serde_json::to_value(*s).ok()?;
                                let art = codec.disk_bytes(&value).ok()?;
                                let ovl = ctx
                                    .overlay
                                    .as_ref()
                                    .and_then(|o| codec.overlay(o, &composite));
                                let json = maybe_strip_overlay(art.json, ovl).ok()?;
                                let formulas: Vec<(String, Vec<u8>)> = art
                                    .sidecars
                                    .into_iter()
                                    .filter_map(|(path, b)| {
                                        let id = path
                                            .strip_prefix("formulas/")
                                            .and_then(|s| s.strip_suffix(".py"))?;
                                        Some((id.to_string(), b))
                                    })
                                    .collect();
                                Some((json, formulas))
                            }) {
                                Some((j, f)) => (Some(j), f),
                                None => (None, Vec::new()),
                            };
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes,
                            restore_code: None,
                            restore_formulas,
                            id: body_pair.as_ref().map(|(s, _)| s.id),
                            modified_at: body_pair
                                .as_ref()
                                .and_then(|(s, _)| s.modified_at().map(|x| x.to_string())),
                            hash_strategy: HashStrategy::Schema,
                        })
                    }
                    "inboxes" => {
                        let body_pair =
                            queue_by_slug
                                .get(it.slug.as_str())
                                .and_then(|(q, ws_slug)| {
                                    catalog
                                        .inboxes_by_queue_id
                                        .get(&q.id)
                                        .map(|i| (i, ws_slug.clone()))
                                });
                        let local_path = match &body_pair {
                            Some((_, ws_slug)) => {
                                ctx.paths.queue_dir(ws_slug, &it.slug).join("inbox.json")
                            }
                            None => crate::cli::push::scan::find_queue_nested_path(
                                ctx.paths,
                                &it.slug,
                                "inbox.json",
                            )
                            .unwrap_or_else(|| {
                                ctx.paths
                                    .workspaces_dir()
                                    .join("__orphan__/queues")
                                    .join(&it.slug)
                                    .join("inbox.json")
                            }),
                        };
                        let restore = body_pair.as_ref().and_then(|(i, _)| {
                            let codec = crate::snapshot::codec::codec("inboxes")?;
                            let value = serde_json::to_value(*i).ok()?;
                            let art = codec.disk_bytes(&value).ok()?;
                            let ovl = ctx
                                .overlay
                                .as_ref()
                                .and_then(|o| codec.overlay(o, &it.slug));
                            let bytes = maybe_strip_overlay(art.json, ovl).ok()?;
                            Some(bytes)
                        });
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: restore,
                            restore_code: None,
                            restore_formulas: Vec::new(),
                            id: body_pair.as_ref().map(|(i, _)| i.id),
                            modified_at: body_pair
                                .as_ref()
                                .and_then(|(i, _)| i.modified_at().map(|x| x.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    "email_templates" => {
                        // Compound slug `<ws>/<q>/<tpl>` — split for path,
                        // look up body via the compound map.
                        let parts: Vec<&str> = it.slug.splitn(3, '/').collect();
                        let local_path = if parts.len() == 3 {
                            ctx.paths
                                .queue_email_templates_dir(parts[0], parts[1])
                                .join(format!("{}.json", parts[2]))
                        } else {
                            ctx.paths
                                .workspaces_dir()
                                .join("__orphan__/email-templates")
                                .join(format!("{}.json", it.slug))
                        };
                        let body = email_template_by_compound.get(it.slug.as_str()).copied();
                        let restore = body.and_then(|t| {
                            let codec = crate::snapshot::codec::codec("email_templates")?;
                            let value = serde_json::to_value(t).ok()?;
                            let art = codec.disk_bytes(&value).ok()?;
                            let ovl = ctx
                                .overlay
                                .as_ref()
                                .and_then(|o| codec.overlay(o, &it.slug));
                            let bytes = maybe_strip_overlay(art.json, ovl).ok()?;
                            Some(bytes)
                        });
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: restore,
                            restore_code: None,
                            restore_formulas: Vec::new(),
                            id: body.map(|t| t.id),
                            modified_at: body.and_then(|t| t.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    other => {
                        progress.event(Action::Warn, &format!(
                            "remote-delete dispatch not yet wired for kind '{}' (slug '{}'); skipping",
                            other, it.slug,
                        ));
                        None
                    }
                };
                let Some(refs) = refs_opt else { continue };
                let local_path = refs.local_path.clone();

                // For LocalDeleteRemoteEdit the local file is tombstoned
                // — restore it from the env-side bytes so the user has
                // something to review on the next sync. The prompt also
                // reads `local_path`, so this restoration is required
                // before the prompt can run.
                if matches!(it.class, SyncClass::LocalDeleteRemoteEdit) {
                    match refs.restore_bytes.as_ref() {
                        Some(bytes) => {
                            if let Some(parent) = local_path.parent() {
                                std::fs::create_dir_all(parent).ok();
                            }
                            write_atomic(&local_path, bytes)?;
                            // Restore the sidecar too for split-file
                            // kinds, so the local restore is byte-complete.
                            // For hooks, the sidecar extension is
                            // derived from the restored JSON's runtime.
                            if matches!(refs.hash_strategy, HashStrategy::Hook | HashStrategy::Rule)
                                && let Some(code) = refs.restore_code.as_ref()
                            {
                                let restored_code_path =
                                    if matches!(refs.hash_strategy, HashStrategy::Hook) {
                                        sidecar_path_for_remote(&local_path, bytes)
                                    } else {
                                        local_path.with_extension("py")
                                    };
                                write_atomic(&restored_code_path, code.as_bytes())?;
                            }
                            // Restore formula sidecars for schemas (bug e fix).
                            if matches!(refs.hash_strategy, HashStrategy::Schema)
                                && !refs.restore_formulas.is_empty()
                            {
                                let queue_dir = local_path.parent().unwrap_or(&local_path);
                                let formulas_dir = queue_dir.join("formulas");
                                std::fs::create_dir_all(&formulas_dir).ok();
                                for (fid, fbytes) in &refs.restore_formulas {
                                    write_atomic(&formulas_dir.join(format!("{fid}.py")), fbytes)?;
                                }
                            }
                        }
                        None => {
                            progress.event(Action::Warn, &format!(
                                "LocalDeleteRemoteEdit for {}/{} but no matching env-side body in catalog; skipping",
                                it.kind, it.slug,
                            ));
                            continue;
                        }
                    }
                }

                if !interactive {
                    // CI / --yes fallback: write the deleted marker so
                    // the next interactive sync re-presents the choice.
                    // For RemoteDelete / LocalEditRemoteDelete the
                    // local file already has bytes; for
                    // LocalDeleteRemoteEdit we just restored it above.
                    if local_path.exists() {
                        let marker = deleted_marker_path(&local_path, &env);
                        write_atomic(&marker, b"")?;
                        progress.event(
                            Action::Warn,
                            &format!(
                                "{}: env deletion deferred (non-tty); marker at {}",
                                local_path.display(),
                                marker.display(),
                            ),
                        );
                    }
                    continue;
                }

                if !local_path.exists() {
                    // RemoteDelete / LocalEditRemoteDelete with no local
                    // file: the classifier saw a tombstone-flavored
                    // state but the file isn't there. Defensive — emit
                    // a warning and move on rather than panic.
                    progress.event(
                        Action::Warn,
                        &format!(
                            "{}: local file missing, cannot prompt; skipping",
                            local_path.display(),
                        ),
                    );
                    continue;
                }

                let prompt_res: std::cell::RefCell<Option<Resolution>> =
                    std::cell::RefCell::new(None);
                progress.with_prompt(|| -> anyhow::Result<()> {
                    let r = prompt_remote_delete(
                        &mut input,
                        std::io::stderr().lock(),
                        &local_path,
                        &env,
                    )?;
                    *prompt_res.borrow_mut() = Some(r);
                    Ok(())
                })?;
                let resolution = prompt_res.into_inner().expect("with_prompt must populate");

                // The action a given letter triggers depends on which
                // class we're resolving (spec §"Double-conflict cases"):
                //   RemoteDelete / LocalEditRemoteDelete:
                //     [k] = restore on env (POST)
                //     [r] = delete local (mirror env's deletion)
                //   LocalDeleteRemoteEdit:
                //     [k] = push DELETE to env (commit the tombstone)
                //     [r] = restore local from env (cancel the tombstone)
                let is_local_delete_remote_edit =
                    matches!(it.class, SyncClass::LocalDeleteRemoteEdit);
                match resolution {
                    Resolution::KeepLocal => {
                        if is_local_delete_remote_edit {
                            // Spec: `[k]` = commit the local tombstone by
                            // DELETEing on env. Sync's executor doesn't
                            // have a DELETE-via-classified path yet (the
                            // push pipeline DELETEs via tombstones, not
                            // ChangeList entries) — defer with a clear
                            // user-facing instruction. The restored
                            // local bytes are removed so re-running the
                            // sync without explicit user intervention
                            // doesn't accidentally re-create the file.
                            std::fs::remove_file(&local_path)
                                .with_context(|| format!("removing {}", local_path.display()))?;
                            progress.event(
                                Action::Info,
                                &format!(
                                    "{}: committing the local tombstone needs an \
                                 explicit `rdc push --allow-deletes {}` follow-up; \
                                 the lockfile entry was retained so the deletion isn't lost",
                                    local_path.display(),
                                    env,
                                ),
                            );
                        } else {
                            // Restore on env: drop the lockfile entry so
                            // the push pipeline's "missing lockfile
                            // entry" branch POSTs the local body as a
                            // new object.
                            drop_lockfile_entry(ctx, &it.kind, &it.slug);
                            outcome.promoted_to_push.push((
                                it.kind.clone(),
                                it.slug.clone(),
                                local_path.clone(),
                            ));
                        }
                    }
                    Resolution::KeepRemote => {
                        if is_local_delete_remote_edit {
                            // Cancel the tombstone: the local file is
                            // already restored from env-side bytes
                            // (done above before prompting). Align the
                            // lockfile to the env hash so subsequent
                            // syncs see Clean state.
                            if let Some(bytes) = refs.restore_bytes.as_ref() {
                                // For schema kinds, use hash_schema so the
                                // formula sidecars are included in the hash.
                                let h = if matches!(refs.hash_strategy, HashStrategy::Schema) {
                                    refs.hash_strategy.hash_schema(
                                        bytes,
                                        &refs.restore_formulas,
                                        &crate::state::Lockfile::default(),
                                    )
                                } else {
                                    refs.hash_strategy.hash(
                                        bytes,
                                        &refs.restore_code,
                                        &crate::state::Lockfile::default(),
                                    )
                                };
                                if let (Some(id), modified_at) = (refs.id, refs.modified_at.clone())
                                {
                                    crate::cli::pull::common::record_object(
                                        ctx.lockfile,
                                        &it.kind,
                                        &it.slug,
                                        id,
                                        modified_at,
                                        Some(h),
                                    );
                                }
                            }
                        } else {
                            // Mirror the env's deletion: remove local +
                            // drop lockfile entry. No push action — env
                            // already doesn't have it. For split-file
                            // kinds, drop the sidecar too — pick the
                            // extension from the local JSON's runtime
                            // before deleting the JSON itself. Also
                            // sweep a stale-other-extension sidecar.
                            let sidecar =
                                sidecar_path_for_conflict(&local_path, refs.hash_strategy);
                            let other_sidecar =
                                if sidecar.extension().and_then(|s| s.to_str()) == Some("js") {
                                    local_path.with_extension("py")
                                } else {
                                    local_path.with_extension("js")
                                };
                            std::fs::remove_file(&local_path)
                                .with_context(|| format!("removing {}", local_path.display()))?;
                            if matches!(refs.hash_strategy, HashStrategy::Hook | HashStrategy::Rule)
                                && sidecar.exists()
                            {
                                std::fs::remove_file(&sidecar)
                                    .with_context(|| format!("removing {}", sidecar.display()))?;
                            }
                            if matches!(refs.hash_strategy, HashStrategy::Hook)
                                && other_sidecar.exists()
                            {
                                std::fs::remove_file(&other_sidecar).with_context(|| {
                                    format!("removing {}", other_sidecar.display())
                                })?;
                            }
                            // For schemas, also sweep the formulas/ directory
                            // so locally-deleted schemas don't leave orphan
                            // formula files behind.
                            if matches!(refs.hash_strategy, HashStrategy::Schema) {
                                let queue_dir = local_path.parent().unwrap_or(&local_path);
                                let formulas_dir = queue_dir.join("formulas");
                                if formulas_dir.exists() {
                                    std::fs::remove_dir_all(&formulas_dir).ok();
                                }
                            }
                            drop_lockfile_entry(ctx, &it.kind, &it.slug);
                        }
                    }
                    Resolution::Skip => {
                        let marker = deleted_marker_path(&local_path, &env);
                        write_atomic(&marker, b"")?;
                        progress.event(
                            Action::Warn,
                            &format!(
                                "{}: env deletion deferred; marker at {}",
                                local_path.display(),
                                marker.display(),
                            ),
                        );
                    }
                    // The remote-delete prompt never offers `[e]` or
                    // `[h]`; the helper's contract documents those
                    // variants as unreachable here. Fall through to
                    // Abort defensively.
                    Resolution::Edit(_) | Resolution::EditWithMarkers(_) | Resolution::Abort => {
                        return Err(anyhow::Error::new(PullAborted));
                    }
                }
            }
            _ => {
                // Not a remote-delete class — skip silently. The other
                // classes are handled by `resolve_conflicts` and the
                // pull / push pipelines.
            }
        }
    }

    Ok(outcome)
}

/// Per-kind refs for one remote-delete-family resolution. `restore_bytes`
/// is `Some` only when the env still has the object body (i.e. the class
/// is `LocalDeleteRemoteEdit` or we just want to make a body available
/// for the prompt's diff viewer). `id`/`url`/`modified_at` come from the
/// same env-side body when present; they're `None` when the env already
/// dropped the object (`RemoteDelete` / `LocalEditRemoteDelete`).
///
/// `restore_code` and `hash_strategy` parallel the `ConflictRefs` shape:
/// split-file kinds (`hooks` / `rules`) carry the sidecar bytes so the
/// resolver can write a complete restore (.json + .py) and use the
/// matching combined-hash helper when it records the lockfile entry.
struct RemoteDeleteRefs {
    local_path: PathBuf,
    restore_bytes: Option<Vec<u8>>,
    restore_code: Option<String>,
    /// Formula sidecars for `HashStrategy::Schema` restores. Each entry
    /// is a `(field_id, bytes)` pair. For non-schema kinds this is empty.
    restore_formulas: Vec<(String, Vec<u8>)>,
    id: Option<u64>,
    modified_at: Option<String>,
    hash_strategy: HashStrategy,
}

/// Find the engine slug that owns a given engine_field slug on disk.
/// Mirrors the disk-sweep used by `push::scan::detect_tombstones` for
/// engine fields; needed by the remote-delete dispatcher when the env
/// already dropped the field (no catalog body → no `engine` URL).
fn find_engine_field_engine_slug(paths: &crate::paths::Paths, field_slug: &str) -> Option<String> {
    let engines_dir = paths.engines_dir();
    if !engines_dir.exists() {
        return None;
    }
    let entries = std::fs::read_dir(&engines_dir).ok()?;
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let candidate = entry
            .path()
            .join("fields")
            .join(format!("{field_slug}.json"));
        if candidate.exists() {
            return Some(entry.file_name().to_string_lossy().to_string());
        }
    }
    None
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
    allow_deletes: bool,
    interactive: bool,
    progress: &Arc<Log>,
) -> Result<crate::cli::sync::CycleOutcome> {
    // Tally cycle counters up front by inspecting the static classification.
    // The dispatch branches below may early-return on user abort or
    // upstream errors; counting here keeps the watch summary aligned with
    // the plan the user saw rather than with whatever the dispatch managed
    // to push through. `no_push` / `no_pull` suppress the corresponding
    // counters so the summary matches what was actually attempted.
    let mut outcome = crate::cli::sync::CycleOutcome::default();
    for it in classified {
        match it.class {
            SyncClass::LocalEdit | SyncClass::LocalCreate | SyncClass::LocalDelete => {
                if !no_push {
                    outcome.items_pushed += 1;
                }
            }
            SyncClass::RemoteEdit | SyncClass::RemoteCreate => {
                if !no_pull {
                    outcome.items_pulled += 1;
                }
            }
            SyncClass::BothDiverged
            | SyncClass::LocalEditRemoteDelete
            | SyncClass::LocalDeleteRemoteEdit => {
                outcome.conflicts += 1;
            }
            SyncClass::RemoteDelete => {
                outcome.remote_deletes_resolved += 1;
            }
            SyncClass::Clean | SyncClass::BothDeleted => {}
        }
    }

    // Phase A: conflicts. Runs first so the user resolves drift before
    // the executor commits to either side. The stdin read here is the
    // only blocking-on-user-input call in the executor; the helper takes
    // a generic `BufRead` so tests can drive it with a `Cursor`.
    //
    // `CoordinatorStdin` (not `stdin().lock()`): in `rdc sync --watch` the
    // Enter-trigger reader is the sole stdin owner and hands prompt input
    // to us through the coordinator; outside watch this reads stdin
    // directly. Either way the read is deferred to the first prompt, so a
    // conflict-free cycle never touches stdin. See `cli::stdin_coord`.
    let conflict_outcome = resolve_conflicts(
        ctx,
        catalog,
        classified,
        CoordinatorStdin::new(),
        interactive,
        progress,
    )
    .await?;

    // Phase A2: remote-delete + double-conflict + both-deleted. Same
    // stdin source as Phase A; `BothDiverged` items have already been
    // resolved above so the dispatcher only sees the destructive-direction
    // classes here.
    let remote_delete_outcome = resolve_remote_deletes(
        ctx,
        catalog,
        classified,
        CoordinatorStdin::new(),
        interactive,
        progress,
    )
    .await?;

    // Phase B: destructive deletes. Items classified as `LocalDelete`
    // (lockfile + remote present, local file gone, remote unchanged since
    // the recorded base) get a `DELETE /<kind>/<id>` here. The order is:
    // build a `Tombstones` from the classified items, gate on
    // `confirm_or_refuse` (prints the full list and prompts on TTY, or
    // refuses non-interactively without `--allow-deletes`), then dispatch
    // `run_deletes` which handles drift detection, cascade order
    // (children before parents), idempotency, and lockfile cleanup.
    //
    // `LocalDeleteRemoteEdit` is intentionally NOT included — it already
    // routed through the remote-delete resolver above. We only act on
    // the clean-delete class here.
    //
    // Gated by `no_push`: audit-mode (`--no-push`) suppresses all
    // outbound mutations, deletes included.
    let mut delete_counts = crate::cli::push::deletes::DeleteCounts::default();
    if !no_push {
        let mut tombstones = crate::cli::push::scan::Tombstones::default();
        for it in classified {
            if !matches!(it.class, SyncClass::LocalDelete) {
                continue;
            }
            let id = ctx
                .lockfile
                .objects
                .get(&it.kind)
                .and_then(|m| m.get(&it.slug))
                .map(|e| e.id);
            let Some(id) = id else {
                // Classifier emits LocalDelete only when the lockfile
                // entry exists; missing here is a logic bug, not a user
                // condition. Skip with a warning rather than panicking
                // mid-sync.
                progress.event(Action::Warn, &format!(
                    "{}/{} classified as LocalDelete but lockfile entry is missing; skipping delete",
                    it.kind, it.slug
                ));
                continue;
            };
            match it.kind.as_str() {
                "workspaces" => {
                    tombstones.workspaces.insert(it.slug.clone(), id);
                }
                "hooks" => {
                    tombstones.hooks.insert(it.slug.clone(), id);
                }
                "rules" => {
                    tombstones.rules.insert(it.slug.clone(), id);
                }
                "labels" => {
                    tombstones.labels.insert(it.slug.clone(), id);
                }
                "queues" => {
                    tombstones.queues.insert(it.slug.clone(), id);
                }
                "schemas" => {
                    tombstones.schemas.insert(it.slug.clone(), id);
                }
                "inboxes" => {
                    tombstones.inboxes.insert(it.slug.clone(), id);
                }
                "email_templates" => {
                    tombstones.email_templates.insert(it.slug.clone(), id);
                }
                "engines" => {
                    tombstones.engines.insert(it.slug.clone(), id);
                }
                "engine_fields" => {
                    tombstones.engine_fields.insert(it.slug.clone(), id);
                }
                other => {
                    progress.event(Action::Warn, &format!(
                        "{other}/{} classified as LocalDelete but kind is not deletable via rdc sync; skipping",
                        it.slug
                    ));
                }
            }
        }
        if !tombstones.is_empty() {
            let confirm_out: std::cell::RefCell<Option<crate::cli::push::deletes::ConfirmOutcome>> =
                std::cell::RefCell::new(None);
            progress.with_prompt(|| -> anyhow::Result<()> {
                let o = crate::cli::push::deletes::confirm_or_refuse(
                    &tombstones,
                    interactive,
                    allow_deletes,
                )?;
                *confirm_out.borrow_mut() = Some(o);
                Ok(())
            })?;
            match confirm_out.into_inner().expect("with_prompt must populate") {
                crate::cli::push::deletes::ConfirmOutcome::Aborted => {
                    progress.event(
                        Action::Info,
                        "delete phase aborted at confirmation; remote unchanged.",
                    );
                }
                crate::cli::push::deletes::ConfirmOutcome::Proceed => {
                    delete_counts = crate::cli::push::deletes::run_deletes(
                        ctx.client,
                        ctx.lockfile,
                        &tombstones,
                        interactive,
                        progress,
                    )
                    .await?;
                }
            }
        }
    }

    // Reconcile `outcome.items_pushed` with what the delete phase actually
    // did. The pre-tally above counted every `LocalDelete` item; subtract
    // the over-count and add back only the deletes that committed (the
    // rest were skipped by drift resolution or aborted at the prompt).
    let local_delete_planned = classified
        .iter()
        .filter(|c| matches!(c.class, SyncClass::LocalDelete))
        .count();
    if !no_push && local_delete_planned > 0 {
        outcome.items_pushed = outcome.items_pushed.saturating_sub(local_delete_planned)
            + delete_counts.total_deleted();
    }

    // Push runs BEFORE pull so the user's local edits land on the remote as
    // soon as the conflict resolver finishes. Pull and push touch disjoint
    // `(kind, slug)` sets (the classifier produces mutually-exclusive
    // classes), so swapping their order does not race them. The per-object
    // drift check inside each push driver (`resolve_push_drift`) and the
    // resolver passes in Phase A/A2 stay unchanged.
    if !no_push {
        // Fold LocalEdit / LocalCreate items into the same `ChangeList`
        // shape `push::scan::scan` produces, then merge in the items
        // the conflict resolver promoted (`[k]eep local` / `[e]dit`) and
        // the remote-delete resolver's restore-on-env promotions (`[k]`
        // on a RemoteDelete / double-conflict — push driver POSTs them
        // because we dropped the lockfile entry).
        let mut change_list =
            crate::cli::push::scan::change_list_from_classified(ctx.paths, classified);
        let promotions = conflict_outcome
            .promoted_to_push
            .into_iter()
            .chain(remote_delete_outcome.promoted_to_push);
        for (kind, slug, path) in promotions {
            match kind.as_str() {
                "labels" => {
                    change_list.labels.insert(slug, path);
                }
                "workspaces" => {
                    change_list.workspaces.insert(slug, path);
                }
                "engines" => {
                    change_list.engines.insert(slug, path);
                }
                "engine_fields" => {
                    change_list.engine_fields.insert(slug, path);
                }
                "hooks" => {
                    change_list.hooks.insert(slug, path);
                }
                "rules" => {
                    change_list.rules.insert(slug, path);
                }
                "queues" => {
                    change_list.queues.insert(slug, path);
                }
                "schemas" => {
                    change_list.schemas.insert(slug, path);
                }
                "inboxes" => {
                    change_list.inboxes.insert(slug, path);
                }
                "email_templates" => {
                    change_list.email_templates.insert(slug, path);
                }
                _ => {}
            }
        }
        // Always run the push pipeline — `hooks::push` may have
        // secrets-only work even when no hook JSON/code changed. The
        // per-kind drivers inside `push_classified` are individually
        // gated on `changes.<kind>.is_empty()`, so dispatching with an
        // empty change list is a no-op for every kind except hooks.
        {
            let env = ctx.paths.env().to_string();
            crate::cli::push::push_classified(
                ctx.paths,
                ctx.client,
                ctx.lockfile,
                &env,
                interactive,
                &change_list,
                &catalog.hooks,
                progress,
            )
            .await?;
        }
    }

    if !no_pull {
        // Group pull-side items by kind so each driver runs at most
        // once per sync. Slugs inside the subset filter through the
        // driver's own `subset.contains(...)` guard.
        //
        // Also include `Clean` items whose lockfile entry is missing
        // (`base_hash` is `None`) — this is the post-`rdc doctor
        // --rebuild-lock` "in sync but no lockfile entry" case. Routing
        // through the pull driver is a safe no-op write (the bytes
        // canonicalize equal) and lets `record_object` rebuild the
        // lockfile entry so the next sync sees it as truly `Clean`.
        let mut subsets: BTreeMap<&str, BTreeSet<(String, String)>> = BTreeMap::new();
        for it in classified {
            let needs_pull_dispatch =
                matches!(it.class, SyncClass::RemoteEdit | SyncClass::RemoteCreate)
                    || (matches!(it.class, SyncClass::Clean) && it.base_hash.is_none());
            if needs_pull_dispatch {
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
            crate::cli::pull::labels::process(ctx, catalog.labels.clone(), subset, progress)
                .await?;
        }

        // organization is a pull-only singleton. The classifier only
        // ever emits "organization"/"self" for RemoteEdit / RemoteCreate
        // (no push side), so any `subsets.get("organization")` hit means
        // we want the driver to write the local file. The driver takes
        // the full Organization rather than a subset filter.
        if subsets.contains_key("organization") {
            crate::cli::pull::organization::process(ctx, catalog.organization.clone(), progress)
                .await?;
        }

        // workflows and workflow_steps are pull-only (read-only at the
        // Rossum API). Each driver respects the `(kind, slug)` subset,
        // so the executor stays a thin dispatcher.
        if let Some(subset) = subsets.get("workflows") {
            crate::cli::pull::workflows::process(ctx, catalog.workflows.clone(), subset, progress)
                .await?;
        }
        if let Some(subset) = subsets.get("workflow_steps") {
            crate::cli::pull::workflow_steps::process(
                ctx,
                catalog.workflow_steps.clone(),
                subset,
                progress,
            )
            .await?;
        }

        // workspaces, engines, engine_fields, mdh — flat push-capable
        // kinds (mdh is pull-only). Each driver already accepts a
        // `(kind, slug)` subset filter; the dispatcher hands off the
        // subset and lets the driver re-derive slugs.
        if let Some(subset) = subsets.get("workspaces") {
            crate::cli::pull::workspaces::process(
                ctx,
                catalog.workspaces.clone(),
                subset,
                progress,
            )
            .await?;
        }
        if let Some(subset) = subsets.get("engines") {
            crate::cli::pull::engines::process(ctx, catalog.engines.clone(), subset, progress)
                .await?;
        }
        if let Some(subset) = subsets.get("engine_fields") {
            crate::cli::pull::engine_fields::process(
                ctx,
                catalog.engine_fields.clone(),
                subset,
                progress,
            )
            .await?;
        }
        if let Some(subset) = subsets.get("hooks") {
            crate::cli::pull::hooks::process(ctx, catalog.hooks.clone(), subset, progress).await?;
        }
        if let Some(subset) = subsets.get("rules") {
            crate::cli::pull::rules::process(ctx, catalog.rules.clone(), subset, progress).await?;
        }
        // queues / schemas / inboxes — the queue pull driver writes all
        // three file types for each queue it processes (queue.json,
        // schema.json + formulas/, inbox.json). The driver's subset
        // filter is keyed by `("queues", slug)`, so we union the
        // schemas/inboxes subsets into the queues subset: any queue
        // whose schema or inbox needs writing pulls the whole tree.
        // This matches the "queue-nested files travel as a unit" rule
        // documented on `pull::queues::process`.
        let mut queue_subset: BTreeSet<(String, String)> =
            subsets.get("queues").cloned().unwrap_or_default();
        for it in classified {
            if matches!(it.class, SyncClass::RemoteEdit | SyncClass::RemoteCreate)
                && (it.kind == "schemas" || it.kind == "inboxes")
            {
                queue_subset.insert(("queues".to_string(), it.slug.clone()));
            }
        }
        if !queue_subset.is_empty() {
            // queues::process also writes `ctx.queue_locations` which the
            // email_templates dispatch below needs. It's a side effect of
            // running the driver.
            crate::cli::pull::queues::process(
                ctx,
                catalog.queues.clone(),
                &catalog.schemas_by_queue_id,
                &catalog.inboxes_by_queue_id,
                &queue_subset,
                progress,
            )
            .await?;
        }
        // email_templates — flat compound slug `<ws>/<q>/<tpl>`. The
        // driver consults `ctx.queue_locations` (populated by the queues
        // dispatch above) to derive the on-disk path.
        if let Some(subset) = subsets.get("email_templates") {
            crate::cli::pull::email_templates::process(
                ctx,
                catalog.email_templates.clone(),
                subset,
                progress,
            )
            .await?;
        }
        // MDH dispatch — runs unconditionally for every server-listed
        // collection (classifier bypass; see catalog setup in
        // `sync::run_cycle`). Two stages:
        //
        // 1. Push: for any dataset whose local indexes.json has drifted
        //    from the lockfile baseline, dispatch the push driver to
        //    drop+create the delta on the server. Gated on `!no_push`
        //    so audit mode (`--no-push`) stays read-only.
        // 2. Pull: idempotent re-read of the now-aligned remote.
        //    Per-file `decide_pull_action` keeps disk bytes stable.
        if !catalog.mdh.collections.is_empty() {
            use std::collections::HashSet as HashSetForSlugs;
            let mut slug_to_collection: BTreeMap<String, &crate::model::Collection> =
                BTreeMap::new();
            let mut used: HashSetForSlugs<String> = HashSetForSlugs::new();
            for c in &catalog.mdh.collections {
                let slug = crate::slug::slugify_unique(&c.name, &used);
                used.insert(slug.clone());
                slug_to_collection.insert(slug, c);
            }

            if !no_push {
                for (slug, collection) in &slug_to_collection {
                    let indexes_path = ctx.paths.dataset_dir(slug).join("indexes.json");
                    if !indexes_path.exists() {
                        continue;
                    }
                    let local_bytes = match std::fs::read(&indexes_path) {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                    let local_hash = crate::state::content_hash(
                        &local_bytes,
                        &crate::state::Lockfile::default(),
                    );
                    let base = ctx
                        .lockfile
                        .objects
                        .get("mdh_indexes")
                        .and_then(|m| m.get(slug.as_str()))
                        .and_then(|e| e.content_hash.as_deref());
                    if Some(local_hash.as_str()) == base {
                        continue;
                    }
                    crate::cli::push::mdh::push_dataset(
                        &catalog.mdh.client,
                        ctx.lockfile,
                        &collection.name,
                        slug,
                        &indexes_path,
                        progress,
                    )
                    .await
                    .with_context(|| format!("pushing local index edits for mdh/{slug}"))?;
                }
            }

            let mut subset: BTreeSet<(String, String)> = BTreeSet::new();
            for slug in slug_to_collection.keys() {
                subset.insert(("mdh".to_string(), slug.clone()));
            }
            let listed = crate::cli::pull::mdh::MdhListed {
                client: catalog.mdh.client.clone(),
                collections: catalog.mdh.collections.clone(),
            };
            crate::cli::pull::mdh::process(ctx, listed, &subset, progress).await?;

            // Reconcile datasets the env no longer lists: an `mdh_indexes`
            // lockfile slug absent from the server's collection set is an
            // orphan (its collection was deleted on the env). MDH bypasses
            // the classifier, so without this the dataset would persist on
            // disk forever. Guarded by the non-empty-collections gate above,
            // so a transient empty / 404 listing (MDH disabled) can never
            // mass-delete every local dataset.
            let remote_slugs: BTreeSet<String> = slug_to_collection.keys().cloned().collect();
            outcome.remote_deletes_resolved += prune_mdh_orphans(
                ctx,
                &remote_slugs,
                CoordinatorStdin::new(),
                interactive,
                progress,
            )
            .await?;
        }

        // Post-pass: rewrite portable-kind URLs in every snapshotted file to
        // `rdc://<kind>/<slug>` form and re-record the lockfile `content_hash`
        // so the next `sync` sees the object as Clean (no phantom drift).
        crate::cli::pull::portabilize::portabilize_refs(ctx.paths, ctx.lockfile)
            .context("portabilizing snapshot references")?;
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::RossumClient;
    use crate::cli::pull::common::RemoteCatalog;
    use crate::model::Label;
    use crate::paths::Paths;
    use crate::state::{Lockfile, ObjectEntry, content_hash};
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
                extra: indexmap::IndexMap::new(),
            },
            workspaces: vec![],
            queues: vec![],
            schemas_by_queue_id: BTreeMap::new(),
            inboxes_by_queue_id: BTreeMap::new(),
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
        let mut extra: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
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
    ///     + Lockfile + a label that exists locally with the BASE bytes and
    ///     has a divergent remote variant. The caller supplies the local
    ///     edit and the remote variant separately to set up the BothDiverged
    ///     state.
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
        let base_hash = content_hash(&base_bytes, &Lockfile::default());

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
                modified_at: None,
                content_hash: Some(base_hash),
                secrets_hash: None,
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
        let local_hash = content_hash(&label_bytes(&fixture.local_edit), &fixture.lockfile);
        let remote_hash = content_hash(&label_bytes(&fixture.remote_label), &fixture.lockfile);
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
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

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
        let remote_hash = content_hash(&remote_bytes, &fixture.lockfile);
        let recorded = fixture
            .lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get("audit-hold"))
            .and_then(|e| e.content_hash.clone())
            .expect("lockfile entry must persist");
        assert_eq!(
            recorded, remote_hash,
            "lockfile base should now equal remote hash"
        );
    }

    /// Scripted `r\n` ([r]use env): the resolver overwrites the local
    /// file with remote bytes, updates the lockfile to the remote hash,
    /// and does NOT promote the item to push (the env wins).
    #[tokio::test]
    async fn resolve_conflicts_use_remote_overwrites_local_and_skips_push() {
        let mut fixture = setup_conflict_fixture();
        let catalog = catalog_with_labels(vec![fixture.remote_label.clone()]);
        let classified = classified_for(&fixture);
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

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

        // No push-side promotion — remote wins, no PATCH needed.
        assert!(
            outcome.promoted_to_push.is_empty(),
            "no push items expected on [r]"
        );

        // Local file overwritten with remote bytes.
        let remote_bytes = label_bytes(&fixture.remote_label);
        let local_after = std::fs::read(&fixture.local_path).unwrap();
        assert_eq!(
            local_after, remote_bytes,
            "local file should be replaced by remote bytes"
        );

        // Lockfile records the remote hash.
        let recorded = fixture
            .lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get("audit-hold"))
            .and_then(|e| e.content_hash.clone())
            .unwrap();
        assert_eq!(recorded, content_hash(&remote_bytes, &fixture.lockfile));
    }

    /// Scripted `s\n` ([s]kip): the resolver writes a shadow file next
    /// to the local one, keeps the local file untouched, and pins the
    /// lockfile to the prior base hash so the next pull/sync
    /// re-classifies the slug as a conflict (otherwise the conflict
    /// gets silently swallowed). No push promotion.
    #[tokio::test]
    async fn resolve_conflicts_skip_writes_shadow_and_preserves_base() {
        let mut fixture = setup_conflict_fixture();
        let prior_base = fixture
            .lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get("audit-hold"))
            .and_then(|e| e.content_hash.clone())
            .expect("fixture must seed a prior base hash");
        let catalog = catalog_with_labels(vec![fixture.remote_label.clone()]);
        let classified = classified_for(&fixture);
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

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

        assert!(
            outcome.promoted_to_push.is_empty(),
            "skip never promotes to push"
        );

        // Shadow file at `<local>.<env>` carries the remote bytes.
        let shadow = crate::paths::shadow_path_for(&fixture.local_path, "test");
        assert!(
            shadow.exists(),
            "shadow file should be written at {}",
            shadow.display()
        );
        let remote_bytes = label_bytes(&fixture.remote_label);
        assert_eq!(std::fs::read(&shadow).unwrap(), remote_bytes);

        // Local file untouched.
        let local_after = std::fs::read(&fixture.local_path).unwrap();
        assert_eq!(
            local_after, local_before,
            "local file must not be modified by [s]"
        );

        // Lockfile entry MUST be pinned to the prior base — advancing it
        // would silently swallow the conflict on the next pull/sync.
        let recorded = fixture
            .lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get("audit-hold"))
            .and_then(|e| e.content_hash.clone())
            .unwrap();
        assert_eq!(
            recorded, prior_base,
            "shadow-skip must preserve the prior lockfile base hash"
        );
    }

    /// Non-interactive (CI / non-TTY) shadow-fallback for a conflict must
    /// also preserve the prior lockfile base. Mirrors the interactive
    /// `[s]` path — the user wasn't even given a chance to resolve, so
    /// the next run must re-surface the conflict.
    #[tokio::test]
    async fn resolve_conflicts_non_interactive_preserves_base() {
        let mut fixture = setup_conflict_fixture();
        let prior_base = fixture
            .lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get("audit-hold"))
            .and_then(|e| e.content_hash.clone())
            .expect("fixture must seed a prior base hash");
        let catalog = catalog_with_labels(vec![fixture.remote_label.clone()]);
        let classified = classified_for(&fixture);
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

        let local_before = std::fs::read(&fixture.local_path).unwrap();

        let outcome = {
            let mut ctx = PullCtx {
                paths: &fixture.paths,
                client: &fixture.client,
                lockfile: &mut fixture.lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: false,
            };
            // Empty stdin — non-interactive path must not block on read.
            resolve_conflicts(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b""),
                false,
                &progress,
            )
            .await
            .expect("non-interactive resolver must succeed")
        };

        assert!(outcome.promoted_to_push.is_empty());

        let shadow = crate::paths::shadow_path_for(&fixture.local_path, "test");
        assert!(shadow.exists(), "non-tty fallback must write a shadow file");

        let local_after = std::fs::read(&fixture.local_path).unwrap();
        assert_eq!(local_after, local_before);

        let recorded = fixture
            .lockfile
            .objects
            .get("labels")
            .and_then(|m| m.get("audit-hold"))
            .and_then(|e| e.content_hash.clone())
            .unwrap();
        assert_eq!(
            recorded, prior_base,
            "non-interactive shadow-fallback must preserve the prior base hash"
        );
    }

    /// Scripted `a\n` ([a]bort): the resolver returns a `PullAborted`
    /// error so the outer driver can detect it and skip lockfile.save().
    #[tokio::test]
    async fn resolve_conflicts_abort_returns_pull_aborted_error() {
        let mut fixture = setup_conflict_fixture();
        let catalog = catalog_with_labels(vec![fixture.remote_label.clone()]);
        let classified = classified_for(&fixture);
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

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

        // Sentinel type lets the outer push/pull runner suppress
        // lockfile.save() — mirrors the apply_pull_action contract.
        assert!(
            err.chain()
                .any(|c| c.downcast_ref::<PullAborted>().is_some()),
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
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);
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

        assert!(outcome.promoted_to_push.is_empty(), "no items to promote");
        assert_eq!(fixture.lockfile, lf_before, "lockfile must be untouched");
    }

    // ----- remote-delete + double-conflict tests (Task 17) ---------------

    /// Scaffolding for a RemoteDelete scenario: a label exists locally
    /// with bytes matching the lockfile base hash, but the env has
    /// dropped it. The classifier produces a `RemoteDelete` item.
    /// Caller drives the resolver and asserts post-state.
    struct RemoteDeleteFixture {
        _tmp: tempfile::TempDir,
        paths: Paths,
        client: RossumClient,
        lockfile: Lockfile,
        local_path: PathBuf,
    }

    fn setup_remote_delete_fixture() -> RemoteDeleteFixture {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.labels_dir()).unwrap();

        // Local label — same bytes as the base recorded in lockfile.
        let local_label = mk_label(42, "Audit Hold", "#aabbcc");
        let local_bytes = label_bytes(&local_label);
        let local_path = paths.labels_dir().join("audit-hold.json");
        std::fs::write(&local_path, &local_bytes).unwrap();
        let base_hash = content_hash(&local_bytes, &Lockfile::default());

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "labels",
            "audit-hold",
            ObjectEntry {
                id: 42,
                modified_at: None,
                content_hash: Some(base_hash),
                secrets_hash: None,
            },
        );

        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();

        RemoteDeleteFixture {
            _tmp: tmp,
            paths,
            client,
            lockfile,
            local_path,
        }
    }

    fn classified_remote_delete() -> Vec<ClassifiedItem> {
        vec![ClassifiedItem {
            kind: "labels".to_string(),
            slug: "audit-hold".to_string(),
            class: SyncClass::RemoteDelete,
            local_hash: None,
            remote_hash: None,
            base_hash: Some("dummy".to_string()),
        }]
    }

    /// Seed `slug` as a clean (local == base) MDH dataset on disk: the
    /// env-tree `indexes.json`, the base-cache mirror, and a lockfile
    /// `mdh_indexes` entry whose `content_hash` matches the bytes.
    /// Returns the base-cache mirror path so callers can assert on it.
    fn seed_mdh_dataset(paths: &Paths, lockfile: &mut Lockfile, slug: &str) -> PathBuf {
        let bytes: &[u8] = b"{\n  \"regular\": [],\n  \"search\": []\n}";
        let dir = paths.dataset_dir(slug);
        std::fs::create_dir_all(&dir).unwrap();
        let ix = dir.join("indexes.json");
        std::fs::write(&ix, bytes).unwrap();
        let mirror = paths
            .base_cache_root()
            .join("mdh")
            .join(slug)
            .join("indexes.json");
        std::fs::create_dir_all(mirror.parent().unwrap()).unwrap();
        std::fs::write(&mirror, bytes).unwrap();
        lockfile.upsert(
            "mdh_indexes",
            slug,
            ObjectEntry {
                id: 0,
                modified_at: None,
                content_hash: Some(content_hash(bytes, &Lockfile::default())),
                secrets_hash: None,
            },
        );
        mirror
    }

    /// Scripted `r\n` ([r] use env; delete local) on an MDH dataset the
    /// remote no longer lists: the orphan's env-tree dir, base-cache
    /// mirror, and lockfile entry are all removed; a sibling dataset the
    /// remote still lists is left completely untouched.
    #[tokio::test]
    async fn prune_mdh_orphans_use_env_removes_dataset_base_and_lockfile() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let mut lockfile = Lockfile::default();

        let live_mirror = seed_mdh_dataset(&paths, &mut lockfile, "vendors");
        let orphan_mirror = seed_mdh_dataset(&paths, &mut lockfile, "vendors-2");

        let client =
            RossumClient::new("https://unused.invalid/api/v1".to_string(), "TEST".to_string())
                .unwrap();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

        // Remote lists only `vendors`; `vendors-2` was deleted on the env.
        let remote_slugs = std::collections::BTreeSet::from(["vendors".to_string()]);

        let pruned = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            prune_mdh_orphans(&mut ctx, &remote_slugs, Cursor::new(b"r\n"), true, &progress)
                .await
                .expect("prune should succeed on [r]")
        };

        assert_eq!(pruned, 1, "exactly one orphan (vendors-2) pruned");

        // Orphan gone everywhere.
        assert!(
            !paths.dataset_dir("vendors-2").exists(),
            "orphan dataset dir must be removed"
        );
        assert!(
            !orphan_mirror.exists(),
            "orphan base-cache mirror must be removed"
        );
        assert!(
            lockfile
                .objects
                .get("mdh_indexes")
                .and_then(|m| m.get("vendors-2"))
                .is_none(),
            "orphan lockfile entry must be dropped"
        );

        // Live dataset untouched.
        assert!(
            paths.dataset_dir("vendors").join("indexes.json").exists(),
            "live dataset dir must survive"
        );
        assert!(live_mirror.exists(), "live base-cache mirror must survive");
        assert!(
            lockfile
                .objects
                .get("mdh_indexes")
                .and_then(|m| m.get("vendors"))
                .is_some(),
            "live lockfile entry must survive"
        );
    }

    /// Non-tty (CI / `--yes`): an MDH orphan must NOT be silently
    /// deleted. The dataset + lockfile entry survive; a `-deleted`
    /// marker is written so the next interactive sync re-presents it.
    #[tokio::test]
    async fn prune_mdh_orphans_non_interactive_defers_with_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let mut lockfile = Lockfile::default();
        seed_mdh_dataset(&paths, &mut lockfile, "vendors-2");

        let client =
            RossumClient::new("https://unused.invalid/api/v1".to_string(), "TEST".to_string())
                .unwrap();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);
        let remote_slugs = std::collections::BTreeSet::<String>::new();

        let indexes_path = paths.dataset_dir("vendors-2").join("indexes.json");
        let pruned = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: false,
            };
            prune_mdh_orphans(&mut ctx, &remote_slugs, Cursor::new(b""), false, &progress)
                .await
                .expect("prune should succeed non-interactively")
        };

        assert_eq!(pruned, 0, "nothing deleted from disk in non-tty mode");
        assert!(indexes_path.exists(), "dataset must survive non-tty");
        assert!(
            lockfile
                .objects
                .get("mdh_indexes")
                .and_then(|m| m.get("vendors-2"))
                .is_some(),
            "lockfile entry must survive non-tty"
        );
        assert!(
            deleted_marker_path(&indexes_path, "test").exists(),
            "a -deleted marker must be written"
        );
    }

    /// Scripted `s\n` ([s]kip): write the marker, keep everything.
    #[tokio::test]
    async fn prune_mdh_orphans_skip_writes_marker_keeps_dataset() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let mut lockfile = Lockfile::default();
        seed_mdh_dataset(&paths, &mut lockfile, "vendors-2");

        let client =
            RossumClient::new("https://unused.invalid/api/v1".to_string(), "TEST".to_string())
                .unwrap();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);
        let remote_slugs = std::collections::BTreeSet::<String>::new();
        let indexes_path = paths.dataset_dir("vendors-2").join("indexes.json");

        let pruned = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            prune_mdh_orphans(&mut ctx, &remote_slugs, Cursor::new(b"s\n"), true, &progress)
                .await
                .expect("prune should succeed on [s]")
        };

        assert_eq!(pruned, 0, "[s] deletes nothing");
        assert!(indexes_path.exists(), "[s] keeps the dataset");
        assert!(
            lockfile
                .objects
                .get("mdh_indexes")
                .and_then(|m| m.get("vendors-2"))
                .is_some(),
            "[s] keeps the lockfile entry"
        );
        assert!(
            deleted_marker_path(&indexes_path, "test").exists(),
            "[s] writes a -deleted marker"
        );
    }

    /// Scripted `k\n` ([k]eep local): MDH can't be re-created on the env,
    /// so keep the on-disk files but DROP the lockfile entry so the
    /// dataset isn't re-flagged every sync.
    #[tokio::test]
    async fn prune_mdh_orphans_keep_local_untracks_but_keeps_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let mut lockfile = Lockfile::default();
        let mirror = seed_mdh_dataset(&paths, &mut lockfile, "vendors-2");

        let client =
            RossumClient::new("https://unused.invalid/api/v1".to_string(), "TEST".to_string())
                .unwrap();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);
        let remote_slugs = std::collections::BTreeSet::<String>::new();
        let indexes_path = paths.dataset_dir("vendors-2").join("indexes.json");

        let pruned = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            prune_mdh_orphans(&mut ctx, &remote_slugs, Cursor::new(b"k\n"), true, &progress)
                .await
                .expect("prune should succeed on [k]")
        };

        assert_eq!(pruned, 0, "[k] deletes nothing from the working tree");
        assert!(indexes_path.exists(), "[k] keeps the local files");
        assert!(
            lockfile
                .objects
                .get("mdh_indexes")
                .and_then(|m| m.get("vendors-2"))
                .is_none(),
            "[k] drops the lockfile entry (stop tracking)"
        );
        assert!(!mirror.exists(), "[k] drops the base-cache mirror");
        assert!(
            !deleted_marker_path(&indexes_path, "test").exists(),
            "[k] writes no marker"
        );
    }

    /// Lockfile entry with no on-disk file: both sides agree on deletion.
    /// Drop the entry silently (no prompt — works non-interactively too).
    #[tokio::test]
    async fn prune_mdh_orphans_missing_local_file_silently_drops_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let mut lockfile = Lockfile::default();
        // Lockfile entry only — no dataset dir / indexes.json on disk.
        lockfile.upsert(
            "mdh_indexes",
            "vendors-2",
            ObjectEntry {
                id: 0,
                modified_at: None,
                content_hash: Some("stale".to_string()),
                secrets_hash: None,
            },
        );

        let client =
            RossumClient::new("https://unused.invalid/api/v1".to_string(), "TEST".to_string())
                .unwrap();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);
        let remote_slugs = std::collections::BTreeSet::<String>::new();

        let pruned = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: false,
            };
            prune_mdh_orphans(&mut ctx, &remote_slugs, Cursor::new(b""), false, &progress)
                .await
                .expect("prune should succeed on both-deleted")
        };

        assert_eq!(pruned, 1, "both-deleted counts as reconciled");
        assert!(
            lockfile
                .objects
                .get("mdh_indexes")
                .and_then(|m| m.get("vendors-2"))
                .is_none(),
            "stale lockfile entry must be dropped silently"
        );
    }

    /// Scripted `k\n` ([k]eep local; restore on env) on a RemoteDelete
    /// label: the resolver must promote a restore item (push pipeline
    /// will POST because the lockfile entry was dropped) and leave the
    /// local file intact.
    #[tokio::test]
    async fn resolve_remote_deletes_keep_local_promotes_to_restore() {
        let mut fixture = setup_remote_delete_fixture();
        let catalog = catalog_with_labels(vec![]);
        let classified = classified_remote_delete();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

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
            resolve_remote_deletes(
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

        // Outcome: one entry promoted as a restore — push pipeline will
        // POST because the lockfile entry was dropped.
        assert_eq!(
            outcome.promoted_to_push.len(),
            1,
            "should promote 1 restore item"
        );
        assert_eq!(outcome.promoted_to_push[0].0, "labels");
        assert_eq!(outcome.promoted_to_push[0].1, "audit-hold");
        assert_eq!(outcome.promoted_to_push[0].2, fixture.local_path);

        // Local file unchanged — push driver re-reads it.
        let local_after = std::fs::read(&fixture.local_path).unwrap();
        assert_eq!(local_after, local_before, "local file must survive [k]");

        // Lockfile entry dropped so push driver treats it as a POST
        // (missing lockfile entry → create-via-POST in `push::labels::push`).
        assert!(
            fixture
                .lockfile
                .objects
                .get("labels")
                .and_then(|m| m.get("audit-hold"))
                .is_none(),
            "lockfile entry must be dropped so push POSTs the restore"
        );
    }

    /// Scripted `r\n` ([r] use env; delete local): the resolver removes
    /// the local file, drops the lockfile entry, does NOT promote to
    /// push (the env's deletion wins).
    #[tokio::test]
    async fn resolve_remote_deletes_use_env_removes_local_and_drops_lockfile() {
        let mut fixture = setup_remote_delete_fixture();
        let catalog = catalog_with_labels(vec![]);
        let classified = classified_remote_delete();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

        let outcome = {
            let mut ctx = PullCtx {
                paths: &fixture.paths,
                client: &fixture.client,
                lockfile: &mut fixture.lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            resolve_remote_deletes(
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

        assert!(
            outcome.promoted_to_push.is_empty(),
            "no push items expected on [r]"
        );
        assert!(
            !fixture.local_path.exists(),
            "local file must be removed by [r]"
        );
        assert!(
            fixture
                .lockfile
                .objects
                .get("labels")
                .and_then(|m| m.get("audit-hold"))
                .is_none(),
            "lockfile entry must be dropped"
        );
    }

    /// Scripted `s\n` ([s]kip): the resolver writes the env-deleted
    /// marker, leaves the local file alone, and retains the lockfile
    /// entry so re-running the sync re-presents the same conflict.
    #[tokio::test]
    async fn resolve_remote_deletes_skip_writes_env_deleted_marker() {
        let mut fixture = setup_remote_delete_fixture();
        let catalog = catalog_with_labels(vec![]);
        let classified = classified_remote_delete();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

        let local_before = std::fs::read(&fixture.local_path).unwrap();
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
            resolve_remote_deletes(
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

        assert!(outcome.promoted_to_push.is_empty(), "skip never promotes");

        // Marker file at `<local>.<env>-deleted` exists.
        let marker = {
            let shadow = crate::paths::shadow_path_for(&fixture.local_path, "test");
            let mut s = shadow.into_os_string();
            s.push("-deleted");
            std::path::PathBuf::from(s)
        };
        assert!(
            marker.exists(),
            "deleted-marker should be written at {}",
            marker.display()
        );

        // Local file untouched.
        let local_after = std::fs::read(&fixture.local_path).unwrap();
        assert_eq!(local_after, local_before, "local file must not be modified");

        // Lockfile untouched — the conflict is deferred, not resolved.
        assert_eq!(fixture.lockfile, lf_before, "lockfile must be unchanged");
    }

    /// `interactive == false`: the resolver writes the env-deleted
    /// marker without reading from stdin, regardless of class.
    #[tokio::test]
    async fn resolve_remote_deletes_non_tty_falls_back_to_skip() {
        let mut fixture = setup_remote_delete_fixture();
        let catalog = catalog_with_labels(vec![]);
        let classified = classified_remote_delete();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

        let local_before = std::fs::read(&fixture.local_path).unwrap();

        let outcome = {
            let mut ctx = PullCtx {
                paths: &fixture.paths,
                client: &fixture.client,
                lockfile: &mut fixture.lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: false,
            };
            // Empty stdin — if the helper tried to read here, the read
            // would return 0 (EOF). Non-TTY path skips the prompt entirely.
            resolve_remote_deletes(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b""),
                false,
                &progress,
            )
            .await
            .expect("non-tty resolver must succeed")
        };

        assert!(outcome.promoted_to_push.is_empty());

        let marker = {
            let shadow = crate::paths::shadow_path_for(&fixture.local_path, "test");
            let mut s = shadow.into_os_string();
            s.push("-deleted");
            std::path::PathBuf::from(s)
        };
        assert!(
            marker.exists(),
            "non-tty fallback must write the deleted-marker"
        );
        let local_after = std::fs::read(&fixture.local_path).unwrap();
        assert_eq!(local_after, local_before, "local file must survive non-tty");
    }

    /// LocalDeleteRemoteEdit: the local file was tombstoned but the
    /// remote still exists with edits. The dispatcher restores the
    /// local file from env-side bytes for review, then prompts. A
    /// scripted `s\n` writes the deleted-marker without removing the
    /// restored local file.
    #[tokio::test]
    async fn resolve_remote_deletes_local_delete_remote_edit_restores_local_for_review() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.labels_dir()).unwrap();
        let local_path = paths.labels_dir().join("audit-hold.json");
        // Local file was deleted by the user (tombstone) — do not write.

        // Env still has the label with a divergent body.
        let remote_label = mk_label(42, "Audit Hold", "#00ff00");
        let remote_bytes = label_bytes(&remote_label);

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "labels",
            "audit-hold",
            ObjectEntry {
                id: 42,
                modified_at: None,
                content_hash: Some("base".to_string()),
                secrets_hash: None,
            },
        );

        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();

        let catalog = catalog_with_labels(vec![remote_label.clone()]);
        let classified = vec![ClassifiedItem {
            kind: "labels".to_string(),
            slug: "audit-hold".to_string(),
            class: SyncClass::LocalDeleteRemoteEdit,
            local_hash: None,
            remote_hash: Some(content_hash(&remote_bytes, &Lockfile::default())),
            base_hash: Some("base".to_string()),
        }];
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

        let outcome = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            resolve_remote_deletes(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b"s\n"),
                true,
                &progress,
            )
            .await
            .expect("LocalDeleteRemoteEdit resolver should succeed on [s]")
        };

        assert!(outcome.promoted_to_push.is_empty());

        // The dispatcher restored the local file from env-side bytes so
        // the user can review it on the next sync run.
        assert!(
            local_path.exists(),
            "local file must be restored from env-side bytes for review"
        );
        assert_eq!(std::fs::read(&local_path).unwrap(), remote_bytes);

        // Marker written too.
        let marker = {
            let shadow = crate::paths::shadow_path_for(&local_path, "test");
            let mut s = shadow.into_os_string();
            s.push("-deleted");
            std::path::PathBuf::from(s)
        };
        assert!(marker.exists(), "deleted-marker should be written");
    }

    /// BothDeleted: no prompt, no marker — just drop the lockfile entry
    /// silently so subsequent syncs see Clean state.
    #[tokio::test]
    async fn both_deleted_silent_drops_lockfile_entry_no_prompt() {
        let mut fixture = setup_remote_delete_fixture();
        // Remove the local file so the class would be BothDeleted in real life.
        std::fs::remove_file(&fixture.local_path).unwrap();

        let catalog = catalog_with_labels(vec![]);
        let classified = vec![ClassifiedItem {
            kind: "labels".to_string(),
            slug: "audit-hold".to_string(),
            class: SyncClass::BothDeleted,
            local_hash: None,
            remote_hash: None,
            base_hash: Some("dummy".to_string()),
        }];
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

        let outcome = {
            let mut ctx = PullCtx {
                paths: &fixture.paths,
                client: &fixture.client,
                lockfile: &mut fixture.lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            // Empty stdin — BothDeleted must not prompt.
            resolve_remote_deletes(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b""),
                true,
                &progress,
            )
            .await
            .expect("BothDeleted resolver must succeed without reading stdin")
        };

        assert!(outcome.promoted_to_push.is_empty());
        assert!(
            fixture
                .lockfile
                .objects
                .get("labels")
                .and_then(|m| m.get("audit-hold"))
                .is_none(),
            "BothDeleted must drop the lockfile entry silently"
        );

        // No marker file should be written for BothDeleted.
        let marker = {
            let shadow = crate::paths::shadow_path_for(&fixture.local_path, "test");
            let mut s = shadow.into_os_string();
            s.push("-deleted");
            std::path::PathBuf::from(s)
        };
        assert!(
            !marker.exists(),
            "BothDeleted must not write a deleted-marker"
        );
    }

    /// Build a minimal RemoteCatalog with the given hooks. Same shape as
    /// `catalog_with_labels` but seeds the hooks slot.
    fn catalog_with_hooks(hooks: Vec<crate::model::Hook>) -> RemoteCatalog {
        let mut c = catalog_with_labels(vec![]);
        c.hooks = hooks;
        c
    }

    /// Helper used by the next two tests. Stages a `BothDiverged` hook
    /// where the JSON portions are byte-identical (post-canonicalize)
    /// but the `.py` sidecars differ on both sides. Returns everything
    /// the caller needs to drive `resolve_conflicts` and verify the
    /// resolution effects.
    fn setup_hook_code_only_conflict() -> (
        tempfile::TempDir,
        Paths,
        Lockfile,
        std::path::PathBuf, /* json path */
        std::path::PathBuf, /* py path */
        RemoteCatalog,
        Vec<ClassifiedItem>,
        String, /* base_combined */
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();

        let slug = "ap-reject-if-no-doc-id";
        let local_json_bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "id": 42,
            "url": "https://x.invalid/api/v1/hooks/42",
            "name": "ap-reject-if-no-doc-id",
            "type": "function",
            "queues": [],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12" }
        }))
        .unwrap();
        let mut local_json_with_newline = local_json_bytes;
        local_json_with_newline.push(b'\n');
        let local_json_path = paths.hooks_dir().join(format!("{slug}.json"));
        let local_py_path = paths.hooks_dir().join(format!("{slug}.py"));
        std::fs::write(&local_json_path, &local_json_with_newline).unwrap();
        std::fs::write(&local_py_path, b"def local_edit():\n    return 2\n").unwrap();

        let remote_hook: crate::model::Hook = serde_json::from_value(serde_json::json!({
            "id": 42,
            "url": "https://x.invalid/api/v1/hooks/42",
            "name": "ap-reject-if-no-doc-id",
            "type": "function",
            "queues": [],
            "events": ["annotation_content"],
            "config": {
                "runtime": "python3.12",
                "code": "def remote_edit():\n    return 3\n"
            },
            "modified_at": "2026-05-14T10:00:00Z"
        }))
        .unwrap();

        let base_code = "def base():\n    return 1\n".to_string();
        let base_combined = crate::state::hook_combined_hash(
            &local_json_with_newline,
            &Some(base_code),
            &Lockfile::default(),
        );
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: 42,
                modified_at: Some("2026-05-14T08:00:00Z".to_string()),
                content_hash: Some(base_combined.clone()),
                secrets_hash: None,
            },
        );

        let catalog = catalog_with_hooks(vec![remote_hook.clone()]);

        let (remote_json_full, remote_code) =
            crate::snapshot::hook::serialize_hook(&remote_hook).unwrap();
        let remote_combined =
            crate::state::hook_combined_hash(&remote_json_full, &remote_code, &Lockfile::default());
        let local_combined = crate::state::hook_combined_hash(
            &local_json_with_newline,
            &Some("def local_edit():\n    return 2\n".to_string()),
            &Lockfile::default(),
        );
        let classified = vec![ClassifiedItem {
            kind: "hooks".to_string(),
            slug: slug.to_string(),
            class: SyncClass::BothDiverged,
            local_hash: Some(local_combined),
            remote_hash: Some(remote_combined.clone()),
            base_hash: Some(base_combined.clone()),
        }];

        (
            tmp,
            paths,
            lockfile,
            local_json_path,
            local_py_path,
            catalog,
            classified,
            base_combined,
        )
    }

    /// Regression: `BothDiverged` for a hook where local and remote
    /// JSON portions are byte-identical (post-canonicalize) but the
    /// `.py` sidecars differ. The resolver MUST prompt — short-
    /// circuiting on JSON equality silently routes the item to
    /// `KeepLocal` (the user's local code overwrites remote on push)
    /// without ever showing the conflict prompt.
    #[tokio::test]
    async fn resolve_conflicts_hook_prompts_when_only_py_sidecar_differs() {
        let (_tmp, paths, mut lockfile, _json_path, py_path, catalog, classified, base_combined) =
            setup_hook_code_only_conflict();
        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

        // Empty stdin → the resolver tries to read a response but
        // must NOT short-circuit silently. With the bug present
        // (JSON-only comparison), the resolver returns `KeepLocal`
        // without reading stdin and promotes the item to push. With
        // the fix, the resolver redirects the prompt to the `.py`
        // sidecar — empty stdin returns `Skip` (the legacy
        // `read_line == 0` fallback) → no promotion, lockfile base
        // preserved.
        let outcome = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            resolve_conflicts(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b""),
                true,
                &progress,
            )
            .await
            .expect("resolver should succeed (even with empty stdin → Skip)")
        };

        assert!(
            outcome.promoted_to_push.is_empty(),
            "hook with only-.py divergence must NOT be silently promoted to push; \
             promoted = {:?}",
            outcome.promoted_to_push,
        );

        let py_after = std::fs::read(&py_path).unwrap();
        assert_eq!(
            py_after, b"def local_edit():\n    return 2\n",
            "local .py edit must survive a conflict-without-prompt"
        );

        let recorded = lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get("ap-reject-if-no-doc-id"))
            .and_then(|e| e.content_hash.clone())
            .expect("lockfile entry must persist");
        assert_eq!(
            recorded, base_combined,
            "lockfile base must remain pinned on Skip/no-prompt; got {recorded}"
        );

        // Shadow file written next to the .py (not the .json) since the
        // prompt was redirected to the code sidecar.
        let shadow = crate::paths::shadow_path_for(&py_path, "test");
        assert!(
            shadow.exists(),
            "shadow file should land next to the .py sidecar: {}",
            shadow.display()
        );
        let shadow_body = std::fs::read(&shadow).unwrap();
        assert_eq!(
            shadow_body, b"def remote_edit():\n    return 3\n",
            "shadow file must carry the remote code, not the JSON"
        );
    }

    /// Same setup as above but the user picks `[k]eep local` interactively.
    /// The resolver must promote the hook to the push pipeline and
    /// pre-align the lockfile to the remote combined hash so the push
    /// driver's drift check passes.
    #[tokio::test]
    async fn resolve_conflicts_hook_keep_local_on_code_conflict_promotes_to_push() {
        let (_tmp, paths, mut lockfile, _json_path, py_path, catalog, classified, _base_combined) =
            setup_hook_code_only_conflict();
        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

        let outcome = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
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

        assert_eq!(
            outcome.promoted_to_push.len(),
            1,
            "[k] must promote the hook to push"
        );
        assert_eq!(outcome.promoted_to_push[0].0, "hooks");
        assert_eq!(outcome.promoted_to_push[0].1, "ap-reject-if-no-doc-id");

        // Local .py must survive — the push driver re-reads it.
        let py_after = std::fs::read(&py_path).unwrap();
        assert_eq!(
            py_after, b"def local_edit():\n    return 2\n",
            "local .py edit must survive [k]"
        );

        // Lockfile base aligned to remote combined hash so the push
        // driver's drift check passes.
        let recorded = lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get("ap-reject-if-no-doc-id"))
            .and_then(|e| e.content_hash.clone())
            .expect("lockfile entry must persist");
        let remote_hook = &catalog.hooks[0];
        let (remote_json_full, remote_code) =
            crate::snapshot::hook::serialize_hook(remote_hook).unwrap();
        let expected = crate::state::hook_combined_hash(&remote_json_full, &remote_code, &lockfile);
        assert_eq!(
            recorded, expected,
            "lockfile base should equal remote combined hash after [k]"
        );
    }

    /// Same setup, but the user picks `[r]use env`. Resolver must adopt
    /// the remote code into the local `.py`, leave the JSON alone (it
    /// was already identical), and NOT promote to push.
    #[tokio::test]
    async fn resolve_conflicts_hook_keep_remote_on_code_conflict_adopts_remote_code() {
        let (_tmp, paths, mut lockfile, json_path, py_path, catalog, classified, _base_combined) =
            setup_hook_code_only_conflict();
        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

        let json_before = std::fs::read(&json_path).unwrap();

        let outcome = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
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

        assert!(outcome.promoted_to_push.is_empty(), "no push on [r]");

        let py_after = std::fs::read(&py_path).unwrap();
        assert_eq!(
            py_after, b"def remote_edit():\n    return 3\n",
            "local .py must be replaced by remote code on [r]"
        );

        // JSON portion was identical canonically — writing the remote
        // JSON over the same content is a no-op effectively. Verify
        // bytes match either before or post-write (both acceptable
        // shapes).
        let json_after = std::fs::read(&json_path).unwrap();
        let json_canon_before =
            crate::snapshot::noise::canonicalize_for_hash(&json_before, &Lockfile::default());
        let json_canon_after =
            crate::snapshot::noise::canonicalize_for_hash(&json_after, &Lockfile::default());
        assert_eq!(
            json_canon_before, json_canon_after,
            "JSON canonical form must remain stable on [r] (was identical before)"
        );
    }

    // ==============================================================
    // Asymmetric & schema BothDiverged tests (Phase 2 of the sync
    // hardening pass). Each test stages a divergent state where the
    // JSON portions canonicalize-equal but the code/formula portions
    // differ in an asymmetric way (one side has code/formula, the
    // other doesn't). Before the resolver hardening, the prompt
    // short-circuited to `KeepLocal` on JSON equality and silently
    // promoted the item to push.
    // ==============================================================

    /// Stage a `BothDiverged` hook where local has a `.py` sidecar
    /// (edited from base) and remote returns the hook WITHOUT
    /// `config.code` (the code was removed remotely). Both sides have
    /// diverged from the lockfile-recorded base (which had code).
    /// `(local_code, remote_code) = (Some, None)` — the prior fix's
    /// `symmetric` redirect doesn't fire here. This was the bug.
    fn setup_hook_local_code_remote_none() -> (
        tempfile::TempDir,
        Paths,
        Lockfile,
        std::path::PathBuf, /* json path */
        std::path::PathBuf, /* py path */
        RemoteCatalog,
        Vec<ClassifiedItem>,
        String, /* base_combined */
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();

        let slug = "ap-validator";
        let local_json_bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "id": 42,
            "url": "https://x.invalid/api/v1/hooks/42",
            "name": "ap-validator",
            "type": "function",
            "queues": [],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12" }
        }))
        .unwrap();
        let mut local_json_with_newline = local_json_bytes;
        local_json_with_newline.push(b'\n');
        let local_json_path = paths.hooks_dir().join(format!("{slug}.json"));
        let local_py_path = paths.hooks_dir().join(format!("{slug}.py"));
        std::fs::write(&local_json_path, &local_json_with_newline).unwrap();
        std::fs::write(&local_py_path, b"def local_edit():\n    return 2\n").unwrap();

        // Remote returns the hook with NO `config.code`. (Rossum's API
        // returns `code: null` typically; we model that as a missing
        // field which the deserializer treats as `None`.)
        let remote_hook: crate::model::Hook = serde_json::from_value(serde_json::json!({
            "id": 42,
            "url": "https://x.invalid/api/v1/hooks/42",
            "name": "ap-validator",
            "type": "function",
            "queues": [],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12" },
            "modified_at": "2026-05-14T10:00:00Z"
        }))
        .unwrap();

        // Base: hook had code = "base" — both sides have since diverged.
        let base_code = "def base():\n    return 1\n".to_string();
        let base_combined = crate::state::hook_combined_hash(
            &local_json_with_newline,
            &Some(base_code),
            &Lockfile::default(),
        );
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: 42,
                modified_at: Some("2026-05-14T08:00:00Z".to_string()),
                content_hash: Some(base_combined.clone()),
                secrets_hash: None,
            },
        );

        let catalog = catalog_with_hooks(vec![remote_hook.clone()]);

        let (remote_json_full, remote_code) =
            crate::snapshot::hook::serialize_hook(&remote_hook).unwrap();
        let remote_combined =
            crate::state::hook_combined_hash(&remote_json_full, &remote_code, &Lockfile::default());
        let local_combined = crate::state::hook_combined_hash(
            &local_json_with_newline,
            &Some("def local_edit():\n    return 2\n".to_string()),
            &Lockfile::default(),
        );
        let classified = vec![ClassifiedItem {
            kind: "hooks".to_string(),
            slug: slug.to_string(),
            class: SyncClass::BothDiverged,
            local_hash: Some(local_combined),
            remote_hash: Some(remote_combined),
            base_hash: Some(base_combined.clone()),
        }];

        (
            tmp,
            paths,
            lockfile,
            local_json_path,
            local_py_path,
            catalog,
            classified,
            base_combined,
        )
    }

    /// Asymmetric: local has code, remote doesn't. With empty stdin
    /// (TTY simulated) the resolver must NOT silently promote — it
    /// MUST prompt. The bug was: JSON portions canonicalize-equal +
    /// asymmetric → falls through to JSON prompt → JSON
    /// canonicalize-equal short-circuit → `KeepLocal` silent push.
    #[tokio::test]
    async fn resolve_conflicts_hook_prompts_when_local_has_code_remote_does_not() {
        let (_tmp, paths, mut lockfile, _json_path, py_path, catalog, classified, base_combined) =
            setup_hook_local_code_remote_none();
        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

        let outcome = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            // Empty stdin → resolver hits the legacy `read_line == 0`
            // fallback, returns `Skip`. NO promotion to push, NO
            // KeepLocal short-circuit. With the bug, the resolver
            // never reads stdin and silently promotes.
            resolve_conflicts(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b""),
                true,
                &progress,
            )
            .await
            .expect("resolver should succeed (Skip on empty stdin)")
        };

        assert!(
            outcome.promoted_to_push.is_empty(),
            "asymmetric (local code, remote none): resolver MUST NOT silently promote; \
             promoted = {:?}",
            outcome.promoted_to_push,
        );

        let py_after = std::fs::read(&py_path).unwrap();
        assert_eq!(
            py_after, b"def local_edit():\n    return 2\n",
            "local .py edit must survive when resolver prompts/skips"
        );

        let recorded = lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get("ap-validator"))
            .and_then(|e| e.content_hash.clone())
            .expect("lockfile entry must persist");
        assert_eq!(
            recorded, base_combined,
            "lockfile base must remain pinned on Skip (next sync re-prompts); got {recorded}"
        );
    }

    /// Mirror of `setup_hook_local_code_remote_none` but local has NO
    /// `.py` (the user deleted it) and remote returns code. Asymmetric
    /// `(local_code, remote_code) = (None, Some)`. Empty stdin →
    /// resolver must prompt/Skip, not silently promote.
    fn setup_hook_local_none_remote_code() -> (
        tempfile::TempDir,
        Paths,
        Lockfile,
        std::path::PathBuf, /* json path */
        RemoteCatalog,
        Vec<ClassifiedItem>,
        String, /* base_combined */
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();

        let slug = "ap-validator";
        let local_json_bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "id": 42,
            "url": "https://x.invalid/api/v1/hooks/42",
            "name": "ap-validator",
            "type": "function",
            "queues": [],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12" }
        }))
        .unwrap();
        let mut local_json_with_newline = local_json_bytes;
        local_json_with_newline.push(b'\n');
        let local_json_path = paths.hooks_dir().join(format!("{slug}.json"));
        std::fs::write(&local_json_path, &local_json_with_newline).unwrap();
        // No .py on disk — user removed it.

        let remote_hook: crate::model::Hook = serde_json::from_value(serde_json::json!({
            "id": 42,
            "url": "https://x.invalid/api/v1/hooks/42",
            "name": "ap-validator",
            "type": "function",
            "queues": [],
            "events": ["annotation_content"],
            "config": {
                "runtime": "python3.12",
                "code": "def remote_edit():\n    return 3\n"
            },
            "modified_at": "2026-05-14T10:00:00Z"
        }))
        .unwrap();

        // Base: hook had code = "base" — both sides have since diverged.
        let base_code = "def base():\n    return 1\n".to_string();
        let base_combined = crate::state::hook_combined_hash(
            &local_json_with_newline,
            &Some(base_code),
            &Lockfile::default(),
        );
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: 42,
                modified_at: Some("2026-05-14T08:00:00Z".to_string()),
                content_hash: Some(base_combined.clone()),
                secrets_hash: None,
            },
        );

        let catalog = catalog_with_hooks(vec![remote_hook.clone()]);

        let (remote_json_full, remote_code) =
            crate::snapshot::hook::serialize_hook(&remote_hook).unwrap();
        let remote_combined =
            crate::state::hook_combined_hash(&remote_json_full, &remote_code, &Lockfile::default());
        // Local has no .py → local hash = `hook_combined_hash(json, None)`.
        let local_combined =
            crate::state::hook_combined_hash(&local_json_with_newline, &None, &Lockfile::default());
        let classified = vec![ClassifiedItem {
            kind: "hooks".to_string(),
            slug: slug.to_string(),
            class: SyncClass::BothDiverged,
            local_hash: Some(local_combined),
            remote_hash: Some(remote_combined),
            base_hash: Some(base_combined.clone()),
        }];

        (
            tmp,
            paths,
            lockfile,
            local_json_path,
            catalog,
            classified,
            base_combined,
        )
    }

    #[tokio::test]
    async fn resolve_conflicts_hook_prompts_when_local_has_no_code_remote_does() {
        let (_tmp, paths, mut lockfile, _json_path, catalog, classified, base_combined) =
            setup_hook_local_none_remote_code();
        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

        let outcome = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            resolve_conflicts(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b""),
                true,
                &progress,
            )
            .await
            .expect("resolver should succeed (Skip on empty stdin)")
        };

        assert!(
            outcome.promoted_to_push.is_empty(),
            "asymmetric (local none, remote code): resolver MUST NOT silently promote; \
             promoted = {:?}",
            outcome.promoted_to_push,
        );

        let recorded = lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get("ap-validator"))
            .and_then(|e| e.content_hash.clone())
            .expect("lockfile entry must persist");
        assert_eq!(
            recorded, base_combined,
            "lockfile base must remain pinned on Skip (next sync re-prompts); got {recorded}"
        );
    }

    /// Schema BothDiverged where the schema JSON canonicalizes equal
    /// across local and remote but the formula sidecars differ (both
    /// sides edited the formula). The classifier emits BothDiverged
    /// because `schema_combined_hash` includes formula bytes. The
    /// resolver currently dispatches with `HashStrategy::Flat` →
    /// `prompt_resolve` sees JSON-canonicalize-equal bytes →
    /// short-circuits → silent KeepLocal promotion. This test pins
    /// the safety property: NO silent promotion on a BothDiverged
    /// schema, regardless of whether the divergence is in the JSON
    /// or in the formulas.
    fn setup_schema_formula_only_conflict() -> (
        tempfile::TempDir,
        Paths,
        Lockfile,
        std::path::PathBuf, /* schema.json path */
        std::path::PathBuf, /* formula path */
        RemoteCatalog,
        Vec<ClassifiedItem>,
        String, /* base_combined */
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let ws_slug = "ap-invoices";
        let q_slug = "cost-invoices";
        let queue_dir = paths.queue_dir(ws_slug, q_slug);
        std::fs::create_dir_all(queue_dir.join("formulas")).unwrap();

        // Build a schema with one formula datapoint.
        let schema_id = 200u64;
        let queue_url = "https://x.invalid/api/v1/queues/100".to_string();
        let schema_url = "https://x.invalid/api/v1/schemas/200".to_string();
        let base_schema: crate::model::Schema = serde_json::from_value(serde_json::json!({
            "id": schema_id,
            "url": schema_url,
            "name": "Cost Invoices Schema",
            "queues": [queue_url.clone()],
            "content": [{
                "category": "section",
                "id": "header",
                "label": "Header",
                "children": [
                    { "category": "datapoint", "id": "invoice_id", "type": "string" },
                    { "category": "datapoint", "id": "amount_total", "type": "number",
                      "formula": "amount_due + amount_tax" }
                ]
            }],
            "modified_at": "2026-04-10T09:00:00Z"
        }))
        .unwrap();

        // The base on-disk: schema.json + formulas/amount_total.py
        // post-extraction. Use serialize_schema to derive the exact
        // bytes the pull driver would have written.
        let (base_json_bytes, base_formulas) =
            crate::snapshot::schema::serialize_schema(&base_schema).unwrap();
        let schema_path = queue_dir.join("schema.json");
        std::fs::write(&schema_path, &base_json_bytes).unwrap();
        let formula_path = queue_dir.join("formulas/amount_total.py");
        std::fs::write(&formula_path, &base_formulas[0].1).unwrap();

        let base_combined = crate::state::schema_combined_hash(
            &base_json_bytes,
            &base_formulas,
            &Lockfile::default(),
        );
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "schemas",
            q_slug,
            ObjectEntry {
                id: schema_id,
                modified_at: Some("2026-04-10T09:00:00Z".to_string()),
                content_hash: Some(base_combined.clone()),
                secrets_hash: None,
            },
        );

        // Local edit: change the formula sidecar.
        std::fs::write(&formula_path, b"amount_due + amount_tax + amount_fee").unwrap();

        // Remote returns the schema with a different formula.
        let remote_schema: crate::model::Schema = serde_json::from_value(serde_json::json!({
            "id": schema_id,
            "url": schema_url,
            "name": "Cost Invoices Schema",
            "queues": [queue_url.clone()],
            "content": [{
                "category": "section",
                "id": "header",
                "label": "Header",
                "children": [
                    { "category": "datapoint", "id": "invoice_id", "type": "string" },
                    { "category": "datapoint", "id": "amount_total", "type": "number",
                      "formula": "amount_due * 1.21" }
                ]
            }],
            "modified_at": "2026-05-14T10:00:00Z"
        }))
        .unwrap();
        let (remote_json_bytes, remote_formulas) =
            crate::snapshot::schema::serialize_schema(&remote_schema).unwrap();
        let remote_combined = crate::state::schema_combined_hash(
            &remote_json_bytes,
            &remote_formulas,
            &Lockfile::default(),
        );

        // Construct a queue + workspace + catalog so resolve_conflicts'
        // schema lookup arm finds the entry.
        let workspace: crate::model::Workspace = serde_json::from_value(serde_json::json!({
            "id": 800,
            "url": "https://x.invalid/api/v1/workspaces/800",
            "name": "AP Invoices",
            "organization": "https://x.invalid/api/v1/organizations/1",
            "queues": [queue_url.clone()],
            "modified_at": "2026-04-20T08:00:00Z"
        }))
        .unwrap();
        let queue: crate::model::Queue = serde_json::from_value(serde_json::json!({
            "id": 100,
            "url": queue_url,
            "name": "Cost Invoices",
            "workspace": "https://x.invalid/api/v1/workspaces/800",
            "schema": schema_url,
            "modified_at": "2026-04-20T08:00:00Z"
        }))
        .unwrap();
        lockfile.upsert(
            "workspaces",
            ws_slug,
            ObjectEntry {
                id: 800,
                modified_at: Some("2026-04-20T08:00:00Z".to_string()),
                content_hash: None,
                secrets_hash: None,
            },
        );
        lockfile.upsert(
            "queues",
            q_slug,
            ObjectEntry {
                id: 100,
                modified_at: Some("2026-04-20T08:00:00Z".to_string()),
                content_hash: None,
                secrets_hash: None,
            },
        );

        let mut catalog = catalog_with_labels(vec![]);
        catalog.workspaces = vec![workspace];
        catalog.queues = vec![queue.clone()];
        catalog.schemas_by_queue_id.insert(queue.id, remote_schema);

        // Local hash uses scan-side formula bytes.
        let mut local_formulas = base_formulas.clone();
        local_formulas[0].1 = b"amount_due + amount_tax + amount_fee".to_vec();
        let local_combined = crate::state::schema_combined_hash(
            &base_json_bytes,
            &local_formulas,
            &Lockfile::default(),
        );

        let classified = vec![ClassifiedItem {
            kind: "schemas".to_string(),
            slug: q_slug.to_string(),
            class: SyncClass::BothDiverged,
            local_hash: Some(local_combined),
            remote_hash: Some(remote_combined),
            base_hash: Some(base_combined.clone()),
        }];

        (
            tmp,
            paths,
            lockfile,
            schema_path,
            formula_path,
            catalog,
            classified,
            base_combined,
        )
    }

    /// Regression for the schema bug: with formula-only divergence on
    /// both sides, the resolver must NOT silently promote the schema
    /// to push. The classifier emits BothDiverged (combined hash
    /// differs), but the resolver's `prompt_resolve` short-circuit on
    /// canonicalize-equal JSON sneaks `KeepLocal` past without ever
    /// reading stdin. This test pins the safety: empty stdin must
    /// yield Skip (no promotion), and the lockfile base must remain
    /// pinned.
    #[tokio::test]
    async fn resolve_conflicts_schema_prompts_when_only_formula_differs() {
        let (
            _tmp,
            paths,
            mut lockfile,
            _schema_path,
            formula_path,
            catalog,
            classified,
            base_combined,
        ) = setup_schema_formula_only_conflict();
        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let progress = Log::new(crate::cli::resolve::ColorMode::Plain);

        let outcome = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            resolve_conflicts(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b""),
                true,
                &progress,
            )
            .await
            .expect("resolver should succeed (Skip on empty stdin)")
        };

        assert!(
            outcome.promoted_to_push.is_empty(),
            "formula-only schema divergence: resolver MUST NOT silently promote; \
             promoted = {:?}",
            outcome.promoted_to_push,
        );

        // Local formula edit must survive.
        let formula_after = std::fs::read(&formula_path).unwrap();
        assert_eq!(
            formula_after, b"amount_due + amount_tax + amount_fee",
            "local formula edit must survive"
        );

        let recorded = lockfile
            .objects
            .get("schemas")
            .and_then(|m| m.get("cost-invoices"))
            .and_then(|e| e.content_hash.clone())
            .expect("lockfile entry must persist");
        assert_eq!(
            recorded, base_combined,
            "lockfile base must remain pinned on Skip (next sync re-prompts); got {recorded}"
        );
    }

    // ----- Bug-d regression: overlay + conflict convergence ---------------
    //
    // An object with an overlay-managed field that resolves a conflict via
    // [r] (keep-remote) must record the POST-overlay hash so the next sync
    // classifies the object as Clean (converges). Before the fix the executor
    // used the PRE-overlay bytes for `remote_bytes`, which produced a hash
    // that never matched the pull driver's on-disk hash → perpetual conflict.

    /// Bug-d repro: hook with an overlay-managed field, BothDiverged
    /// conflict, resolved via `[r]` (keep-remote). The lockfile must
    /// record `combined_hash(post_overlay_json, code_sidecar)`, not the
    /// pre-overlay bytes, so the NEXT sync classifies the hook as Clean.
    #[tokio::test]
    async fn resolve_conflicts_hook_overlay_keep_remote_records_post_overlay_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();

        let slug = "validator-hook";

        // Hook served by remote: has a `description` field managed by the overlay.
        let remote_hook: crate::model::Hook = serde_json::from_value(serde_json::json!({
            "id": 555,
            "url": "https://x.invalid/api/v1/hooks/555",
            "name": "Validator hook",
            "type": "function",
            "queues": [],
            "events": ["annotation_content"],
            "config": {
                "runtime": "python3.12",
                "code": "def validate(payload):\n    pass\n"
            },
            "description": "PROD-specific description managed by overlay"
        }))
        .unwrap();

        // Local file: same hook but user edited `events` (adds annotation_status).
        // The overlay is in effect, so on-disk the file has no `description`.
        let local_json_no_desc = serde_json::json!({
            "id": 555,
            "url": "https://x.invalid/api/v1/hooks/555",
            "name": "Validator hook",
            "type": "function",
            "queues": [],
            "events": ["annotation_content", "annotation_status"],
            "config": { "runtime": "python3.12" }
        });
        let mut local_json_bytes = serde_json::to_vec_pretty(&local_json_no_desc).unwrap();
        local_json_bytes.push(b'\n');
        let local_json_path = paths.hooks_dir().join(format!("{slug}.json"));
        let local_py_path = paths.hooks_dir().join(format!("{slug}.py"));
        std::fs::write(&local_json_path, &local_json_bytes).unwrap();
        std::fs::write(&local_py_path, b"def validate(payload):\n    pass\n").unwrap();

        // Build the overlay that strips `description` from hooks/<slug>.
        let overlay_toml = format!(
            "version = 1\n\n[hooks.{}]\n\"description\" = \"PROD-specific description managed by overlay\"\n",
            slug
        );
        let overlay: crate::overlay::Overlay = toml::from_str(&overlay_toml).unwrap();

        // Seed the lockfile with a base hash that differs from both local and
        // remote (simulates a prior state before both sides diverged).
        // Build lockfile first so all hash computations can use it for URL
        // normalization (rdc:// form), matching what the resolver records.
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: 555,
                modified_at: None,
                content_hash: Some(String::new()), // placeholder, replaced below
                secrets_hash: None,
            },
        );

        let base_json = serde_json::json!({
            "id": 555,
            "url": "https://x.invalid/api/v1/hooks/555",
            "name": "Validator hook",
            "type": "function",
            "queues": [],
            "events": ["annotation_content"],
            "config": { "runtime": "python3.12" }
        });
        let mut base_bytes = serde_json::to_vec_pretty(&base_json).unwrap();
        base_bytes.push(b'\n');
        let base_code = "def validate(payload):\n    pass\n".to_string();
        let base_hash = combined_hash(
            &base_bytes,
            &[("code".to_string(), base_code.into_bytes())],
            &lockfile,
        );
        // Update the lockfile entry with the real base hash.
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: 555,
                modified_at: None,
                content_hash: Some(base_hash.clone()),
                secrets_hash: None,
            },
        );

        // Compute the expected "correct" remote hash: post-overlay (no description)
        // + code sidecar. This is what the pull driver records and what the next
        // classifier would compute on the on-disk bytes.
        let codec = crate::snapshot::codec::codec("hooks").unwrap();
        let remote_value = serde_json::to_value(&remote_hook).unwrap();
        let remote_art = codec.disk_bytes(&remote_value).unwrap();
        let remote_ovl = overlay.hook(slug);
        let remote_json_stripped =
            crate::cli::pull::common::maybe_strip_overlay(remote_art.json, remote_ovl).unwrap();
        let remote_code_bytes = remote_art
            .sidecars
            .iter()
            .find(|(k, _)| k == "code")
            .map(|(_, b)| b.clone())
            .unwrap_or_default();
        let expected_hash = combined_hash(
            &remote_json_stripped,
            &[("code".to_string(), remote_code_bytes)],
            &lockfile,
        );

        // Simulate the local hash: post-overlay (no description) + local code.
        let local_code_str = "def validate(payload):\n    pass\n".to_string();
        let local_hash = combined_hash(
            &local_json_bytes,
            &[("code".to_string(), local_code_str.into_bytes())],
            &lockfile,
        );

        // Remote hash via the codec (what the classifier would compute).
        let remote_hash = expected_hash.clone();
        let classified = vec![ClassifiedItem {
            kind: "hooks".to_string(),
            slug: slug.to_string(),
            class: SyncClass::BothDiverged,
            local_hash: Some(local_hash),
            remote_hash: Some(remote_hash),
            base_hash: Some(base_hash),
        }];

        let catalog = catalog_with_hooks(vec![remote_hook]);
        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);

        // Resolve with `[r]` (keep-remote).
        let outcome = {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
                queue_locations: BTreeMap::new(),
                overlay: Some(overlay),
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
            .expect("resolver must succeed on [r]")
        };

        // No push promotion — remote wins.
        assert!(
            outcome.promoted_to_push.is_empty(),
            "keep-remote must not promote to push"
        );

        // The recorded lockfile hash MUST be the post-overlay combined hash.
        // Before the fix, this would be the pre-overlay hash (containing
        // `description`), which would cause the next sync to re-conflict forever.
        let recorded = lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(slug))
            .and_then(|e| e.content_hash.clone())
            .expect("lockfile must have entry after resolution");
        assert_eq!(
            recorded, expected_hash,
            "bug-d: keep-remote must record the POST-overlay combined hash; \
             before the fix the executor wrote the pre-overlay hash causing perpetual conflict. \
             got {recorded}, expected {expected_hash}"
        );

        // The on-disk file must be the post-overlay bytes (no `description`).
        let disk_json = std::fs::read_to_string(&local_json_path).unwrap();
        assert!(
            !disk_json.contains("description"),
            "on-disk hook must not contain the overlay-managed field after keep-remote; got: {disk_json}"
        );
    }

    // ----- Bug-e regression: schema restore with formulas -----------------
    //
    // A schema that is locally deleted and remotely edited with formula
    // sidecars must be fully restored (schema.json + formulas/*.py) when
    // the user chooses [r] (cancel tombstone / use env). Before the fix the
    // executor dropped the formula sidecars and used HashStrategy::Flat,
    // producing an incomplete restore and a wrong lockfile hash.

    /// Build a minimal catalog that has a workspace + queue + schema with
    /// one formula datapoint. Returns the catalog, the schema model, and
    /// the queue/workspace models for lockfile seeding.
    fn schema_restore_catalog() -> (
        RemoteCatalog,
        crate::model::Schema,
        crate::model::Queue,
        crate::model::Workspace,
    ) {
        let ws: crate::model::Workspace = serde_json::from_value(serde_json::json!({
            "id": 900,
            "url": "https://x.invalid/api/v1/workspaces/900",
            "name": "AP Invoices WS",
            "organization": "https://x.invalid/api/v1/organizations/1",
            "queues": ["https://x.invalid/api/v1/queues/200"],
            "modified_at": "2026-04-20T08:00:00Z"
        }))
        .unwrap();
        let queue: crate::model::Queue = serde_json::from_value(serde_json::json!({
            "id": 200,
            "url": "https://x.invalid/api/v1/queues/200",
            "name": "Cost Invoices Q",
            "workspace": "https://x.invalid/api/v1/workspaces/900",
            "schema": "https://x.invalid/api/v1/schemas/300",
            "modified_at": "2026-04-20T08:00:00Z"
        }))
        .unwrap();
        let schema: crate::model::Schema = serde_json::from_value(serde_json::json!({
            "id": 300,
            "url": "https://x.invalid/api/v1/schemas/300",
            "name": "Cost Invoices Schema",
            "queues": ["https://x.invalid/api/v1/queues/200"],
            "content": [{
                "category": "section",
                "id": "header",
                "label": "Header",
                "children": [
                    { "category": "datapoint", "id": "invoice_id", "type": "string" },
                    { "category": "datapoint", "id": "amount_total", "type": "number",
                      "formula": "amount_due + amount_tax" }
                ]
            }]
        }))
        .unwrap();

        let mut catalog = catalog_with_labels(vec![]);
        catalog.workspaces = vec![ws.clone()];
        catalog.queues = vec![queue.clone()];
        catalog.schemas_by_queue_id.insert(queue.id, schema.clone());

        (catalog, schema, queue, ws)
    }

    /// Bug-e repro: schema with ≥1 formula that is locally deleted and
    /// remotely edited. On `[r]` (cancel tombstone / use env), both
    /// schema.json AND formulas/<field_id>.py must be written, and the
    /// lockfile hash must be `combined_hash(json, framed_sidecars)` so
    /// the NEXT sync sees Clean (no re-conflict).
    #[tokio::test]
    async fn resolve_remote_deletes_schema_restore_writes_formula_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let (catalog, schema, queue, ws) = schema_restore_catalog();

        let ws_slug = "ap-invoices-ws";
        let q_slug = "cost-invoices-q";
        let queue_dir = paths.queue_dir(ws_slug, q_slug);
        std::fs::create_dir_all(&queue_dir).unwrap();

        let schema_json_path = queue_dir.join("schema.json");
        let formula_path = queue_dir.join("formulas/amount_total.py");

        // Schema was locally deleted (file doesn't exist on disk).
        // Remote still has it with edits (the formula exists on remote).

        // Seed lockfile: entry exists (was there before user deleted it).
        let codec = crate::snapshot::codec::codec("schemas").unwrap();
        let schema_value = serde_json::to_value(&schema).unwrap();
        let schema_art = codec.disk_bytes(&schema_value).unwrap();
        let schema_hash =
            combined_hash(&schema_art.json, &schema_art.sidecars, &Lockfile::default());

        let mut lockfile = Lockfile::default();
        // Workspace + queue entries so the slug resolver finds them.
        lockfile.upsert(
            "workspaces",
            ws_slug,
            ObjectEntry {
                id: ws.id,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lockfile.upsert(
            "queues",
            q_slug,
            ObjectEntry {
                id: queue.id,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lockfile.upsert(
            "schemas",
            q_slug,
            ObjectEntry {
                id: schema.id,
                modified_at: None,
                content_hash: Some(schema_hash),
                secrets_hash: None,
            },
        );

        // The classifier would emit LocalDeleteRemoteEdit:
        let classified = vec![ClassifiedItem {
            kind: "schemas".to_string(),
            slug: q_slug.to_string(),
            class: SyncClass::LocalDeleteRemoteEdit,
            local_hash: None,
            remote_hash: None,
            base_hash: None,
        }];

        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let progress = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);

        // Resolve with `[r]` (cancel tombstone — use env version).
        {
            let mut ctx = PullCtx {
                paths: &paths,
                client: &client,
                lockfile: &mut lockfile,
                queue_locations: BTreeMap::new(),
                overlay: None,
                interactive: true,
            };
            resolve_remote_deletes(
                &mut ctx,
                &catalog,
                &classified,
                Cursor::new(b"r\n"),
                true,
                &progress,
            )
            .await
            .expect("resolver must succeed on [r]");
        }

        // Bug-e fix: schema.json must exist after restore.
        assert!(
            schema_json_path.exists(),
            "bug-e: schema.json must be restored on [r] (cancel tombstone)"
        );

        // Bug-e fix: formula sidecar must be restored too.
        assert!(
            formula_path.exists(),
            "bug-e: formulas/amount_total.py must be restored on [r]; before the fix formula \
             sidecars were dropped from the restore"
        );
        let formula_content = std::fs::read_to_string(&formula_path).unwrap();
        assert_eq!(
            formula_content, "amount_due + amount_tax",
            "restored formula must match the remote schema's formula"
        );

        // The lockfile hash must be combined_hash(post-overlay json, framed sidecars)
        // so the next sync sees Clean.
        let expected_hash = codec
            .base_hash(&schema_value, &Lockfile::default())
            .unwrap();
        let recorded = lockfile
            .objects
            .get("schemas")
            .and_then(|m| m.get(q_slug))
            .and_then(|e| e.content_hash.clone())
            .expect("lockfile must have schema entry after restore");
        assert_eq!(
            recorded, expected_hash,
            "bug-e: restored schema hash must be combined_hash(json, framed_sidecars); \
             before the fix it used HashStrategy::Flat which dropped formulas from the hash. \
             got {recorded}, expected {expected_hash}"
        );
    }
}
