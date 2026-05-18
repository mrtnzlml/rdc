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

use crate::cli::pull::common::{PullCtx, RemoteCatalog};
use crate::cli::resolve::{prompt_remote_delete, prompt_resolve, PullAborted, Resolution};
use crate::cli::sync::classify::{ClassifiedItem, SyncClass};
use crate::progress::ProgressLog;
use crate::slug::slugify_unique;
use crate::snapshot::writer::write_atomic;
use crate::state::content_hash;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
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
    progress: &Arc<ProgressLog>,
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
    {
        let mut used: HashSet<String> = HashSet::new();
        for w in &catalog.workspaces {
            let slug = match ctx.lockfile.slug_for_id("workspaces", w.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&w.name, &used),
            };
            used.insert(slug.clone());
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
    let mut engine_field_by_slug: BTreeMap<String, &crate::model::EngineField> = BTreeMap::new();
    {
        let mut used: HashSet<String> = HashSet::new();
        for f in &catalog.engine_fields {
            let slug = match ctx.lockfile.slug_for_id("engine_fields", f.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&f.name, &used),
            };
            used.insert(slug.clone());
            engine_field_by_slug.insert(slug, f);
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
        let mut per_ws: std::collections::HashMap<String, HashSet<String>> =
            std::collections::HashMap::new();
        for q in &catalog.queues {
            let Some(ws_url) = q.workspace.as_ref() else { continue };
            let ws_slug = match ctx.lockfile.slug_for_url("workspaces", ws_url) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let used = per_ws.entry(ws_slug.clone()).or_default();
            let q_slug = match ctx.lockfile.slug_for_id("queues", q.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&q.name, used),
            };
            used.insert(q_slug.clone());
            q_url_to_ws_q.insert(q.url.clone(), (ws_slug.clone(), q_slug.clone()));
            queue_by_slug.insert(q_slug, (q, ws_slug));
        }
    }
    let email_template_by_compound: BTreeMap<String, &crate::model::EmailTemplate> = {
        let mut map: BTreeMap<String, &crate::model::EmailTemplate> = BTreeMap::new();
        let mut per_q: std::collections::HashMap<(String, String), HashSet<String>> =
            std::collections::HashMap::new();
        for t in &catalog.email_templates {
            let Some(queue_url) = t.queue.as_ref() else { continue };
            let Some((ws_slug, q_slug)) = q_url_to_ws_q.get(queue_url).cloned() else { continue };
            let used = per_q.entry((ws_slug.clone(), q_slug.clone())).or_default();
            let template_slug = match ctx.lockfile.slug_for_id("email_templates", t.id) {
                Some(existing) => existing
                    .rsplit('/')
                    .next()
                    .unwrap_or(existing)
                    .to_string(),
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
                let mut bytes = match serde_json::to_vec_pretty(l) {
                    Ok(b) => b,
                    Err(_) => return None,
                };
                bytes.push(b'\n');
                let local_path = ctx.paths.labels_dir().join(format!("{}.json", it.slug));
                Some(ConflictRefs {
                    remote_bytes: bytes,
                    remote_code: None,
                    remote_formulas: Vec::new(),
                    local_path,
                    id: l.id,
                    url: Some(l.url.clone()),
                    modified_at: l.modified_at().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Flat,
                })
            }),
            "workspaces" => workspace_by_slug.get(it.slug.as_str()).copied().and_then(|w| {
                let mut bytes = match serde_json::to_vec_pretty(w) {
                    Ok(b) => b,
                    Err(_) => return None,
                };
                bytes.push(b'\n');
                let local_path = ctx.paths.workspace_dir(&it.slug).join("workspace.json");
                Some(ConflictRefs {
                    remote_bytes: bytes,
                    remote_code: None,
                    remote_formulas: Vec::new(),
                    local_path,
                    id: w.id,
                    url: Some(w.url.clone()),
                    modified_at: w.modified_at().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Flat,
                })
            }),
            "engines" => engine_by_slug.get(it.slug.as_str()).copied().and_then(|e| {
                let mut bytes = match serde_json::to_vec_pretty(e) {
                    Ok(b) => b,
                    Err(_) => return None,
                };
                bytes.push(b'\n');
                let local_path = ctx.paths.engine_dir(&it.slug).join("engine.json");
                Some(ConflictRefs {
                    remote_bytes: bytes,
                    remote_code: None,
                    remote_formulas: Vec::new(),
                    local_path,
                    id: e.id,
                    url: Some(e.url.clone()),
                    modified_at: e.modified_at().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Flat,
                })
            }),
            "engine_fields" => {
                engine_field_by_slug.get(it.slug.as_str()).copied().and_then(|f| {
                    let mut bytes = match serde_json::to_vec_pretty(f) {
                        Ok(b) => b,
                        Err(_) => return None,
                    };
                    bytes.push(b'\n');
                    // Engine fields live under their parent engine; use the
                    // lockfile's id → engine slug mapping (same as the pull
                    // driver). Missing parent → defensive skip.
                    let engine_slug = ctx
                        .lockfile
                        .slug_for_url("engines", &f.engine)
                        .map(|s| s.to_string())?;
                    let local_path = ctx
                        .paths
                        .engine_fields_dir(&engine_slug)
                        .join(format!("{}.json", it.slug));
                    Some(ConflictRefs {
                        remote_bytes: bytes,
                        remote_code: None,
                        remote_formulas: Vec::new(),
                        local_path,
                        id: f.id,
                        url: Some(f.url.clone()),
                        modified_at: f.modified_at().map(|s| s.to_string()),
                        hash_strategy: HashStrategy::Flat,
                    })
                })
            }
            "hooks" => hook_by_slug.get(it.slug.as_str()).copied().and_then(|h| {
                // Reproduce the pull driver's canonical bytes: serialize
                // → strip `config.code` into `code` → trailing newline.
                let (json_bytes, code) = match crate::snapshot::hook::serialize_hook(h) {
                    Ok(pair) => pair,
                    Err(_) => return None,
                };
                let local_path = ctx.paths.hooks_dir().join(format!("{}.json", it.slug));
                Some(ConflictRefs {
                    remote_bytes: json_bytes,
                    remote_code: code,
                    remote_formulas: Vec::new(),
                    local_path,
                    id: h.id,
                    url: Some(h.url.clone()),
                    modified_at: h.modified_at().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Hook,
                })
            }),
            "rules" => rule_by_slug.get(it.slug.as_str()).copied().and_then(|r| {
                let (json_bytes, code) = match crate::snapshot::rule::serialize_rule(r) {
                    Ok(pair) => pair,
                    Err(_) => return None,
                };
                let local_path = ctx.paths.rules_dir().join(format!("{}.json", it.slug));
                Some(ConflictRefs {
                    remote_bytes: json_bytes,
                    remote_code: code,
                    remote_formulas: Vec::new(),
                    local_path,
                    id: r.id,
                    url: Some(r.url.clone()),
                    modified_at: r.modified_at().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Rule,
                })
            }),
            "queues" => queue_by_slug.get(it.slug.as_str()).and_then(|(q, ws_slug)| {
                let mut bytes = match serde_json::to_vec_pretty(*q) {
                    Ok(b) => b,
                    Err(_) => return None,
                };
                bytes.push(b'\n');
                let local_path = ctx.paths.queue_dir(ws_slug, &it.slug).join("queue.json");
                Some(ConflictRefs {
                    remote_bytes: bytes,
                    remote_code: None,
                    remote_formulas: Vec::new(),
                    local_path,
                    id: q.id,
                    url: Some(q.url.clone()),
                    modified_at: q.modified_at().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Flat,
                })
            }),
            "schemas" => {
                // Schemas use `HashStrategy::Schema` so the resolver
                // hashes the canonical schema_combined_hash (json +
                // formulas) into the lockfile. Without this, a
                // formula-only divergence would record only the JSON
                // hash and either (a) silently round-trip via
                // KeepLocal short-circuit on JSON canonicalize-equal
                // bytes, or (b) leave the lockfile in a corrupt state
                // where the recorded hash doesn't match what the
                // classifier next computes.
                queue_by_slug.get(it.slug.as_str()).and_then(|(q, ws_slug)| {
                    let schema = catalog.schemas_by_queue_id.get(&q.id)?;
                    let (json_bytes, formulas) =
                        crate::snapshot::schema::serialize_schema(schema).ok()?;
                    let local_path = ctx.paths.queue_dir(ws_slug, &it.slug).join("schema.json");
                    Some(ConflictRefs {
                        remote_bytes: json_bytes,
                        remote_code: None,
                        remote_formulas: formulas,
                        local_path,
                        id: schema.id,
                        url: Some(schema.url.clone()),
                        modified_at: schema.modified_at().map(|s| s.to_string()),
                        hash_strategy: HashStrategy::Schema,
                    })
                })
            }
            "inboxes" => queue_by_slug.get(it.slug.as_str()).and_then(|(q, ws_slug)| {
                let inbox = catalog.inboxes_by_queue_id.get(&q.id)?;
                let mut bytes = match serde_json::to_vec_pretty(inbox) {
                    Ok(b) => b,
                    Err(_) => return None,
                };
                bytes.push(b'\n');
                let local_path = ctx.paths.queue_dir(ws_slug, &it.slug).join("inbox.json");
                Some(ConflictRefs {
                    remote_bytes: bytes,
                    remote_code: None,
                    remote_formulas: Vec::new(),
                    local_path,
                    id: inbox.id,
                    url: Some(inbox.url.clone()),
                    modified_at: inbox.modified_at().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Flat,
                })
            }),
            "email_templates" => {
                email_template_by_compound.get(it.slug.as_str()).copied().and_then(|t| {
                    let mut bytes = match serde_json::to_vec_pretty(t) {
                        Ok(b) => b,
                        Err(_) => return None,
                    };
                    bytes.push(b'\n');
                    // Compound slug split: `<ws>/<q>/<tpl>`.
                    let parts: Vec<&str> = it.slug.splitn(3, '/').collect();
                    if parts.len() != 3 {
                        return None;
                    }
                    let local_path = ctx
                        .paths
                        .queue_email_templates_dir(parts[0], parts[1])
                        .join(format!("{}.json", parts[2]));
                    Some(ConflictRefs {
                        remote_bytes: bytes,
                        remote_code: None,
                        remote_formulas: Vec::new(),
                        local_path,
                        id: t.id,
                        url: Some(t.url.clone()),
                        modified_at: t.modified_at().map(|s| s.to_string()),
                        hash_strategy: HashStrategy::Flat,
                    })
                })
            }
            other => {
                progress.warn(format!(
                    "warning: conflict resolver not yet wired for kind '{}' (slug '{}'); skipping",
                    other, it.slug,
                ));
                None
            }
        }) else {
            // No catalog entry / orphan / unwired kind — warn and move on.
            progress.warn(format!(
                "warning: conflict for {}/{} but no matching remote object found; skipping",
                it.kind, it.slug,
            ));
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
    url: Option<String>,
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
    /// For `Schema`, `code` is unused (the formulas sidecar list is
    /// derived from the queue dir by the caller); use `hash_schema`
    /// instead.
    fn hash(self, json_bytes: &[u8], code: &Option<String>) -> String {
        match self {
            HashStrategy::Flat => content_hash(json_bytes),
            HashStrategy::Hook => crate::state::hook_combined_hash(json_bytes, code),
            HashStrategy::Rule => crate::state::rule_combined_hash(json_bytes, code),
            // For Schema the caller must use `hash_schema` so the
            // formulas sidecar list is included. Falling back to the
            // bare json-only hash here would silently drop the
            // formulas from the canonical hash — exactly the bug
            // this strategy exists to fix.
            HashStrategy::Schema => content_hash(json_bytes),
        }
    }

    /// Compute the canonical lockfile hash for schema items, including
    /// formulas. Use this for `HashStrategy::Schema` instead of `hash`.
    fn hash_schema(self, json_bytes: &[u8], formulas: &[(String, Vec<u8>)]) -> String {
        debug_assert!(matches!(self, HashStrategy::Schema));
        crate::state::schema_combined_hash(json_bytes, formulas)
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
    progress: &Arc<ProgressLog>,
    outcome: &mut ConflictOutcome,
) -> Result<()> {
    let ConflictRefs {
        remote_bytes,
        remote_code,
        remote_formulas,
        local_path,
        id,
        url,
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
    let (local_code, local_formulas): (Option<String>, Vec<(String, Vec<u8>)>) =
        match hash_strategy {
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
                let formulas = crate::snapshot::schema::read_local_formulas(queue_dir)
                    .unwrap_or_default();
                (None, formulas)
            }
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
    let local_json_canon = crate::snapshot::noise::canonicalize_for_hash(&local_json_bytes);
    let remote_json_canon = crate::snapshot::noise::canonicalize_for_hash(&remote_bytes);
    let json_canonicalize_equal = local_json_canon == remote_json_canon;
    let sidecar_diverges = match hash_strategy {
        HashStrategy::Hook | HashStrategy::Rule => {
            local_code.as_deref() != remote_code.as_deref()
        }
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
        HashStrategy::Schema => hash_strategy.hash_schema(&remote_bytes, &remote_formulas),
        _ => hash_strategy.hash(&remote_bytes, &remote_code),
    };
    let canonical_local_hash = match hash_strategy {
        HashStrategy::Schema => hash_strategy.hash_schema(&local_json_bytes, &local_formulas),
        HashStrategy::Hook | HashStrategy::Rule => {
            hash_strategy.hash(&local_json_bytes, &local_code)
        }
        HashStrategy::Flat => hash_strategy.hash(&local_json_bytes, &None),
    };

    // Defensive sanity: if the canonical hashes already match, the
    // classifier was confused (e.g. by a transient catalog reorder).
    // Treat as Clean — record the matching hash and don't promote.
    if canonical_local_hash == canonical_remote_hash {
        crate::cli::pull::common::record_object(
            ctx.lockfile,
            &it.kind,
            &it.slug,
            id,
            url,
            modified_at,
            Some(canonical_local_hash),
        );
        return Ok(());
    }

    if !interactive {
        // Non-TTY/--yes: fall back to legacy shadow-file behavior so the
        // run still completes without blocking on stdin. The local file
        // stays as-is and the lockfile is pinned to the prior base —
        // advancing it would let the conflict silently disappear on
        // subsequent runs. When there is no prior base (post-`rdc repair
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
        let (shadow_anchor, shadow_bytes): (PathBuf, Vec<u8>) =
            if json_canonicalize_equal && sidecar_diverges {
                match hash_strategy {
                    HashStrategy::Hook | HashStrategy::Rule => {
                        let remote_code_bytes =
                            remote_code.clone().unwrap_or_default().into_bytes();
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
                        let representative = remote_formulas
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
                                local_formulas.iter().find(|(id, _)| {
                                    !remote_formulas.iter().any(|(rid, _)| rid == id)
                                })
                            })
                            .map(|(fid, bytes)| {
                                (
                                    formulas_dir.join(format!("{fid}.py")),
                                    bytes.clone(),
                                )
                            })
                            .unwrap_or_else(|| (local_path.clone(), remote_bytes.clone()));
                        representative
                    }
                    HashStrategy::Flat => (local_path.clone(), remote_bytes.clone()),
                }
            } else {
                (local_path.clone(), remote_bytes.clone())
            };
        let conflict_path = crate::paths::shadow_path_for(&shadow_anchor, env);
        write_atomic(&conflict_path, &shadow_bytes)?;
        progress.warn(format!(
            "warn: {} conflict: local preserved, remote at {} (lockfile base preserved; re-run to resolve)",
            shadow_anchor.display(),
            conflict_path.display(),
        ));
        let preserved_hash: Option<String> = it.base_hash.clone();
        crate::cli::pull::common::record_object(
            ctx.lockfile,
            &it.kind,
            &it.slug,
            id,
            url,
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
    let (resolution, code_conflict_only, prompt_local_bytes, prompt_remote_bytes, prompt_path)
        : (Resolution, bool, Vec<u8>, Vec<u8>, PathBuf) =
    {
        if json_canonicalize_equal && sidecar_diverges {
            match hash_strategy {
                HashStrategy::Hook | HashStrategy::Rule => {
                    let local_bytes = local_code.clone().unwrap_or_default().into_bytes();
                    let remote_bytes_for_prompt =
                        remote_code.clone().unwrap_or_default().into_bytes();
                    let r = crate::cli::resolve::prompt_resolve_with_bytes(
                        input,
                        stderr_lock,
                        idx_one_based,
                        total,
                        &code_path,
                        &local_bytes,
                        &remote_bytes_for_prompt,
                        env,
                    )?;
                    (r, true, local_bytes, remote_bytes_for_prompt, code_path.clone())
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
                                chosen = Some((rid.clone(), lb.unwrap_or_default(), rbytes.clone()));
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
                        input,
                        stderr_lock,
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
                input,
                stderr_lock,
                idx_one_based,
                total,
                &local_path,
                &remote_bytes,
                env,
            )?;
            (r, false, local_json_bytes.clone(), remote_bytes.clone(), local_path.clone())
        }
    };

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
                url,
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
                    std::fs::remove_file(&code_path).with_context(|| {
                        format!("removing stale {}", code_path.display())
                    })?;
                }
            } else if matches!(hash_strategy, HashStrategy::Schema) {
                // Adopt full remote formulas dir: write every remote
                // formula and remove any local formula not present in
                // remote.
                let queue_dir = local_path.parent().unwrap_or(&local_path);
                let formulas_dir = queue_dir.join("formulas");
                if !remote_formulas.is_empty() {
                    std::fs::create_dir_all(&formulas_dir).with_context(|| {
                        format!("creating {}", formulas_dir.display())
                    })?;
                }
                for (fid, bytes) in &remote_formulas {
                    write_atomic(
                        &formulas_dir.join(format!("{fid}.py")),
                        bytes,
                    )?;
                }
                // Sweep formulas that exist locally but not remotely.
                for (lid, _) in &local_formulas {
                    if !remote_formulas.iter().any(|(rid, _)| rid == lid) {
                        let stale = formulas_dir.join(format!("{lid}.py"));
                        if stale.exists() {
                            std::fs::remove_file(&stale).with_context(|| {
                                format!("removing stale {}", stale.display())
                            })?;
                        }
                    }
                }
            }
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                url,
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
            let edit_target = if code_conflict_only { &code_path } else { &local_path };
            if let Some(parent) = edit_target.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            write_atomic(edit_target, &edited)?;
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                url,
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
            let edit_target = if code_conflict_only { &code_path } else { &local_path };
            if let Some(parent) = edit_target.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            write_atomic(edit_target, &edited)?;
            progress.warn(format!(
                "warn: {} partially resolved (markers retained); lockfile base preserved; re-run to resolve",
                edit_target.display(),
            ));
            // When base is absent (post-`rdc repair --rebuild-lock`, or any
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
                url,
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
            progress.warn(format!(
                "warn: {} conflict: local preserved, remote at {} (lockfile base preserved; re-run to resolve)",
                prompt_path.display(),
                conflict_path.display(),
            ));
            // When base is absent (post-`rdc repair --rebuild-lock`, or any
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
                url,
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
            if let Ok(bytes) = std::fs::read(local_path) {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    let ext = crate::snapshot::hook::hook_code_extension_from_value(&v);
                    return local_path.with_extension(ext);
                }
            }
            local_path.with_extension("py")
        }
        // Rules and Flat — `trigger_condition` is always Python.
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
    progress: &Arc<ProgressLog>,
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
    {
        let mut used: HashSet<String> = HashSet::new();
        for w in &catalog.workspaces {
            let slug = match ctx.lockfile.slug_for_id("workspaces", w.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&w.name, &used),
            };
            used.insert(slug.clone());
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
    let mut engine_field_by_slug: BTreeMap<String, &crate::model::EngineField> = BTreeMap::new();
    {
        let mut used: HashSet<String> = HashSet::new();
        for f in &catalog.engine_fields {
            let slug = match ctx.lockfile.slug_for_id("engine_fields", f.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&f.name, &used),
            };
            used.insert(slug.clone());
            engine_field_by_slug.insert(slug, f);
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
        let mut per_ws: std::collections::HashMap<String, HashSet<String>> =
            std::collections::HashMap::new();
        for q in &catalog.queues {
            let Some(ws_url) = q.workspace.as_ref() else { continue };
            let ws_slug = match ctx.lockfile.slug_for_url("workspaces", ws_url) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let used = per_ws.entry(ws_slug.clone()).or_default();
            let q_slug = match ctx.lockfile.slug_for_id("queues", q.id) {
                Some(existing) => existing.to_string(),
                None => slugify_unique(&q.name, used),
            };
            used.insert(q_slug.clone());
            q_url_to_ws_q.insert(q.url.clone(), (ws_slug.clone(), q_slug.clone()));
            queue_by_slug.insert(q_slug, (q, ws_slug));
        }
    }
    let email_template_by_compound: BTreeMap<String, &crate::model::EmailTemplate> = {
        let mut map: BTreeMap<String, &crate::model::EmailTemplate> = BTreeMap::new();
        let mut per_q: std::collections::HashMap<(String, String), HashSet<String>> =
            std::collections::HashMap::new();
        for t in &catalog.email_templates {
            let Some(queue_url) = t.queue.as_ref() else { continue };
            let Some((ws_slug, q_slug)) = q_url_to_ws_q.get(queue_url).cloned() else { continue };
            let used = per_q.entry((ws_slug.clone(), q_slug.clone())).or_default();
            let template_slug = match ctx.lockfile.slug_for_id("email_templates", t.id) {
                Some(existing) => existing
                    .rsplit('/')
                    .next()
                    .unwrap_or(existing)
                    .to_string(),
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
                    progress.warn(format!(
                        "warning: BothDeleted handler not yet wired for kind '{}' (slug '{}'); skipping",
                        it.kind, it.slug,
                    ));
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
                        let local_path =
                            ctx.paths.labels_dir().join(format!("{}.json", it.slug));
                        let body = label_by_slug.get(it.slug.as_str()).copied();
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: body
                                .and_then(|l| serde_json::to_vec_pretty(l).ok())
                                .map(|mut b| {
                                    b.push(b'\n');
                                    b
                                }),
                            restore_code: None,
                            id: body.map(|l| l.id),
                            url: body.map(|l| l.url.clone()),
                            modified_at: body.and_then(|l| l.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    "workspaces" => {
                        let local_path =
                            ctx.paths.workspace_dir(&it.slug).join("workspace.json");
                        let body = workspace_by_slug.get(it.slug.as_str()).copied();
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: body
                                .and_then(|w| serde_json::to_vec_pretty(w).ok())
                                .map(|mut b| {
                                    b.push(b'\n');
                                    b
                                }),
                            restore_code: None,
                            id: body.map(|w| w.id),
                            url: body.map(|w| w.url.clone()),
                            modified_at: body.and_then(|w| w.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    "engines" => {
                        let local_path = ctx.paths.engine_dir(&it.slug).join("engine.json");
                        let body = engine_by_slug.get(it.slug.as_str()).copied();
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: body
                                .and_then(|e| serde_json::to_vec_pretty(e).ok())
                                .map(|mut b| {
                                    b.push(b'\n');
                                    b
                                }),
                            restore_code: None,
                            id: body.map(|e| e.id),
                            url: body.map(|e| e.url.clone()),
                            modified_at: body.and_then(|e| e.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    "engine_fields" => {
                        let body = engine_field_by_slug.get(it.slug.as_str()).copied();
                        // For LocalDeleteRemoteEdit / RemoteDelete, the
                        // local path depends on which engine owns the
                        // field. Walk the lockfile / catalog mapping to
                        // find it.
                        let engine_slug_opt = body
                            .and_then(|f| ctx.lockfile.slug_for_url("engines", &f.engine))
                            .map(|s| s.to_string())
                            .or_else(|| {
                                // No catalog body (env-side dropped the
                                // field): fall back to disk sweep using
                                // the existing scanner helper.
                                find_engine_field_engine_slug(ctx.paths, &it.slug)
                            });
                        let local_path = match engine_slug_opt {
                            Some(es) => ctx
                                .paths
                                .engine_fields_dir(&es)
                                .join(format!("{}.json", it.slug)),
                            // Fallback path — won't exist, prompt will
                            // skip via the `!local_path.exists()` guard.
                            None => ctx
                                .paths
                                .engines_dir()
                                .join("__orphan__/fields")
                                .join(format!("{}.json", it.slug)),
                        };
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: body
                                .and_then(|f| serde_json::to_vec_pretty(f).ok())
                                .map(|mut b| {
                                    b.push(b'\n');
                                    b
                                }),
                            restore_code: None,
                            id: body.map(|f| f.id),
                            url: body.map(|f| f.url.clone()),
                            modified_at: body.and_then(|f| f.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    "hooks" => {
                        let local_path =
                            ctx.paths.hooks_dir().join(format!("{}.json", it.slug));
                        let body = hook_by_slug.get(it.slug.as_str()).copied();
                        // serialize_hook splits the JSON from `config.code`
                        // — same canonical form the pull driver writes.
                        let serialized =
                            body.and_then(|h| crate::snapshot::hook::serialize_hook(h).ok());
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: serialized.as_ref().map(|(b, _)| b.clone()),
                            restore_code: serialized.as_ref().and_then(|(_, c)| c.clone()),
                            id: body.map(|h| h.id),
                            url: body.map(|h| h.url.clone()),
                            modified_at: body.and_then(|h| h.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Hook,
                        })
                    }
                    "rules" => {
                        let local_path =
                            ctx.paths.rules_dir().join(format!("{}.json", it.slug));
                        let body = rule_by_slug.get(it.slug.as_str()).copied();
                        let serialized =
                            body.and_then(|r| crate::snapshot::rule::serialize_rule(r).ok());
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: serialized.as_ref().map(|(b, _)| b.clone()),
                            restore_code: serialized.as_ref().and_then(|(_, c)| c.clone()),
                            id: body.map(|r| r.id),
                            url: body.map(|r| r.url.clone()),
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
                            None => crate::cli::push::scan::find_queue_nested_path(ctx.paths, &it.slug, "queue.json")
                                .unwrap_or_else(|| {
                                    ctx.paths
                                        .workspaces_dir()
                                        .join("__orphan__/queues")
                                        .join(&it.slug)
                                        .join("queue.json")
                                }),
                        };
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: body
                                .and_then(|(q, _)| serde_json::to_vec_pretty(*q).ok())
                                .map(|mut b| {
                                    b.push(b'\n');
                                    b
                                }),
                            restore_code: None,
                            id: body.map(|(q, _)| q.id),
                            url: body.map(|(q, _)| q.url.clone()),
                            modified_at: body.and_then(|(q, _)| q.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    "schemas" => {
                        // schemas live alongside the queue at schema.json.
                        let body_pair = queue_by_slug.get(it.slug.as_str()).and_then(|(q, ws_slug)| {
                            catalog
                                .schemas_by_queue_id
                                .get(&q.id)
                                .map(|s| (s, ws_slug.clone()))
                        });
                        let local_path = match &body_pair {
                            Some((_, ws_slug)) => {
                                ctx.paths.queue_dir(ws_slug, &it.slug).join("schema.json")
                            }
                            None => crate::cli::push::scan::find_queue_nested_path(ctx.paths, &it.slug, "schema.json")
                                .unwrap_or_else(|| {
                                    ctx.paths
                                        .workspaces_dir()
                                        .join("__orphan__/queues")
                                        .join(&it.slug)
                                        .join("schema.json")
                                }),
                        };
                        // Use flat hash strategy for the dispatcher
                        // simplification (display only schema.json; formula
                        // sidecars are not restored here on [k]). The
                        // resolver's hash-into-lockfile call uses Flat so
                        // it stays a single-file decision.
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: body_pair
                                .as_ref()
                                .and_then(|(s, _)| crate::snapshot::schema::serialize_schema(s).ok())
                                .map(|(json, _formulas)| json),
                            restore_code: None,
                            id: body_pair.as_ref().map(|(s, _)| s.id),
                            url: body_pair.as_ref().map(|(s, _)| s.url.clone()),
                            modified_at: body_pair
                                .as_ref()
                                .and_then(|(s, _)| s.modified_at().map(|x| x.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    "inboxes" => {
                        let body_pair = queue_by_slug.get(it.slug.as_str()).and_then(|(q, ws_slug)| {
                            catalog
                                .inboxes_by_queue_id
                                .get(&q.id)
                                .map(|i| (i, ws_slug.clone()))
                        });
                        let local_path = match &body_pair {
                            Some((_, ws_slug)) => {
                                ctx.paths.queue_dir(ws_slug, &it.slug).join("inbox.json")
                            }
                            None => crate::cli::push::scan::find_queue_nested_path(ctx.paths, &it.slug, "inbox.json")
                                .unwrap_or_else(|| {
                                    ctx.paths
                                        .workspaces_dir()
                                        .join("__orphan__/queues")
                                        .join(&it.slug)
                                        .join("inbox.json")
                                }),
                        };
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: body_pair
                                .as_ref()
                                .and_then(|(i, _)| serde_json::to_vec_pretty(*i).ok())
                                .map(|mut b| {
                                    b.push(b'\n');
                                    b
                                }),
                            restore_code: None,
                            id: body_pair.as_ref().map(|(i, _)| i.id),
                            url: body_pair.as_ref().map(|(i, _)| i.url.clone()),
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
                        Some(RemoteDeleteRefs {
                            local_path,
                            restore_bytes: body
                                .and_then(|t| serde_json::to_vec_pretty(t).ok())
                                .map(|mut b| {
                                    b.push(b'\n');
                                    b
                                }),
                            restore_code: None,
                            id: body.map(|t| t.id),
                            url: body.map(|t| t.url.clone()),
                            modified_at: body.and_then(|t| t.modified_at().map(|s| s.to_string())),
                            hash_strategy: HashStrategy::Flat,
                        })
                    }
                    other => {
                        progress.warn(format!(
                            "warning: remote-delete dispatch not yet wired for kind '{}' (slug '{}'); skipping",
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
                            if matches!(
                                refs.hash_strategy,
                                HashStrategy::Hook | HashStrategy::Rule
                            ) {
                                if let Some(code) = refs.restore_code.as_ref() {
                                    let restored_code_path =
                                        if matches!(refs.hash_strategy, HashStrategy::Hook) {
                                            sidecar_path_for_remote(&local_path, bytes)
                                        } else {
                                            local_path.with_extension("py")
                                        };
                                    write_atomic(&restored_code_path, code.as_bytes())?;
                                }
                            }
                        }
                        None => {
                            progress.warn(format!(
                                "warning: LocalDeleteRemoteEdit for {}/{} but no matching env-side body in catalog; skipping",
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
                        progress.warn(format!(
                            "warn: {}: env deletion deferred (non-tty); marker at {}",
                            local_path.display(),
                            marker.display(),
                        ));
                    }
                    continue;
                }

                if !local_path.exists() {
                    // RemoteDelete / LocalEditRemoteDelete with no local
                    // file: the classifier saw a tombstone-flavored
                    // state but the file isn't there. Defensive — emit
                    // a warning and move on rather than panic.
                    progress.warn(format!(
                        "warn: {}: local file missing, cannot prompt; skipping",
                        local_path.display(),
                    ));
                    continue;
                }

                let resolution = prompt_remote_delete(
                    &mut input,
                    std::io::stderr().lock(),
                    &local_path,
                    &env,
                )?;

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
                            std::fs::remove_file(&local_path).with_context(|| {
                                format!("removing {}", local_path.display())
                            })?;
                            progress.warn(format!(
                                "note: {}: committing the local tombstone needs an \
                                 explicit `rdc push --allow-deletes {}` follow-up; \
                                 the lockfile entry was retained so the deletion isn't lost",
                                local_path.display(),
                                env,
                            ));
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
                                let h = refs.hash_strategy.hash(bytes, &refs.restore_code);
                                if let (Some(id), url, modified_at) =
                                    (refs.id, refs.url.clone(), refs.modified_at.clone())
                                {
                                    crate::cli::pull::common::record_object(
                                        ctx.lockfile,
                                        &it.kind,
                                        &it.slug,
                                        id,
                                        url,
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
                            let sidecar = sidecar_path_for_conflict(
                                &local_path,
                                refs.hash_strategy,
                            );
                            let other_sidecar = if sidecar.extension().and_then(|s| s.to_str())
                                == Some("js")
                            {
                                local_path.with_extension("py")
                            } else {
                                local_path.with_extension("js")
                            };
                            std::fs::remove_file(&local_path).with_context(|| {
                                format!("removing {}", local_path.display())
                            })?;
                            if matches!(
                                refs.hash_strategy,
                                HashStrategy::Hook | HashStrategy::Rule
                            ) && sidecar.exists()
                            {
                                std::fs::remove_file(&sidecar).with_context(|| {
                                    format!("removing {}", sidecar.display())
                                })?;
                            }
                            if matches!(refs.hash_strategy, HashStrategy::Hook)
                                && other_sidecar.exists()
                            {
                                std::fs::remove_file(&other_sidecar).with_context(|| {
                                    format!("removing {}", other_sidecar.display())
                                })?;
                            }
                            drop_lockfile_entry(ctx, &it.kind, &it.slug);
                        }
                    }
                    Resolution::Skip => {
                        let marker = deleted_marker_path(&local_path, &env);
                        write_atomic(&marker, b"")?;
                        progress.warn(format!(
                            "warn: {}: env deletion deferred; marker at {}",
                            local_path.display(),
                            marker.display(),
                        ));
                    }
                    // The remote-delete prompt never offers `[e]` or
                    // `[h]`; the helper's contract documents those
                    // variants as unreachable here. Fall through to
                    // Abort defensively.
                    Resolution::Edit(_)
                    | Resolution::EditWithMarkers(_)
                    | Resolution::Abort => {
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
    id: Option<u64>,
    url: Option<String>,
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
    progress: &Arc<ProgressLog>,
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
    let stdin = std::io::stdin();
    let conflict_outcome =
        resolve_conflicts(ctx, catalog, classified, stdin.lock(), interactive, progress).await?;

    // Phase A2: remote-delete + double-conflict + both-deleted. Same
    // stdin source as Phase A; `BothDiverged` items have already been
    // resolved above so the dispatcher only sees the destructive-direction
    // classes here.
    let remote_delete_outcome =
        resolve_remote_deletes(ctx, catalog, classified, stdin.lock(), interactive, progress)
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
                eprintln!(
                    "warning: {}/{} classified as LocalDelete but lockfile entry is missing; skipping delete",
                    it.kind, it.slug
                );
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
                    eprintln!(
                        "warning: {other}/{} classified as LocalDelete but kind is not deletable via rdc sync; skipping",
                        it.slug
                    );
                }
            }
        }
        if !tombstones.is_empty() {
            match crate::cli::push::deletes::confirm_or_refuse(
                &tombstones,
                interactive,
                allow_deletes,
            )? {
                crate::cli::push::deletes::ConfirmOutcome::Aborted => {
                    eprintln!("delete phase aborted at confirmation; remote unchanged.");
                }
                crate::cli::push::deletes::ConfirmOutcome::Proceed => {
                    let phase = progress.phase("deleting");
                    delete_counts = crate::cli::push::deletes::run_deletes(
                        ctx.client,
                        ctx.lockfile,
                        &tombstones,
                        interactive,
                    )
                    .await?;
                    drop(phase);
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
        outcome.items_pushed = outcome
            .items_pushed
            .saturating_sub(local_delete_planned)
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
        // (`base_hash` is `None`) — this is the post-`rdc repair
        // --rebuild-lock` "in sync but no lockfile entry" case. Routing
        // through the pull driver is a safe no-op write (the bytes
        // canonicalize equal) and lets `record_object` rebuild the
        // lockfile entry so the next sync sees it as truly `Clean`.
        let mut subsets: BTreeMap<&str, BTreeSet<(String, String)>> = BTreeMap::new();
        for it in classified {
            let needs_pull_dispatch = matches!(it.class, SyncClass::RemoteEdit | SyncClass::RemoteCreate)
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
            crate::cli::pull::labels::process(ctx, catalog.labels.clone(), subset, progress).await?;
        }

        // organization is a pull-only singleton. The classifier only
        // ever emits "organization"/"self" for RemoteEdit / RemoteCreate
        // (no push side), so any `subsets.get("organization")` hit means
        // we want the driver to write the local file. The driver takes
        // the full Organization rather than a subset filter.
        if subsets.get("organization").is_some() {
            crate::cli::pull::organization::process(ctx, catalog.organization.clone(), progress).await?;
        }

        // workflows and workflow_steps are pull-only (read-only at the
        // Rossum API). Each driver respects the `(kind, slug)` subset,
        // so the executor stays a thin dispatcher.
        if let Some(subset) = subsets.get("workflows") {
            crate::cli::pull::workflows::process(ctx, catalog.workflows.clone(), subset, progress).await?;
        }
        if let Some(subset) = subsets.get("workflow_steps") {
            crate::cli::pull::workflow_steps::process(ctx, catalog.workflow_steps.clone(), subset, progress).await?;
        }

        // workspaces, engines, engine_fields, mdh — flat push-capable
        // kinds (mdh is pull-only). Each driver already accepts a
        // `(kind, slug)` subset filter; the dispatcher hands off the
        // subset and lets the driver re-derive slugs.
        if let Some(subset) = subsets.get("workspaces") {
            crate::cli::pull::workspaces::process(ctx, catalog.workspaces.clone(), subset, progress).await?;
        }
        if let Some(subset) = subsets.get("engines") {
            crate::cli::pull::engines::process(ctx, catalog.engines.clone(), subset, progress).await?;
        }
        if let Some(subset) = subsets.get("engine_fields") {
            crate::cli::pull::engine_fields::process(ctx, catalog.engine_fields.clone(), subset, progress).await?;
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
        let mut queue_subset: BTreeSet<(String, String)> = subsets
            .get("queues")
            .cloned()
            .unwrap_or_default();
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
            crate::cli::pull::queues::process(ctx, catalog.queues.clone(), &queue_subset, progress).await?;
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
        if let Some(subset) = subsets.get("mdh") {
            // MDH lives in a separate listed shape (`MdhListed` carries
            // the Data Storage client + collection list). The pull
            // driver consumes it by value, so we clone the constituent
            // pieces here. `MdhListed` doesn't impl Clone, so we
            // reconstruct it from the catalog's client + collections.
            let listed = crate::cli::pull::mdh::MdhListed {
                client: catalog.mdh.client.clone(),
                collections: catalog.mdh.collections.clone(),
            };
            crate::cli::pull::mdh::process(ctx, listed, subset, progress).await?;
        }
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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

        assert!(outcome.promoted_to_push.is_empty(), "skip never promotes to push");

        // Shadow file at `<local>.<env>` carries the remote bytes.
        let shadow = crate::paths::shadow_path_for(&fixture.local_path, "test");
        assert!(shadow.exists(), "shadow file should be written at {}", shadow.display());
        let remote_bytes = label_bytes(&fixture.remote_label);
        assert_eq!(std::fs::read(&shadow).unwrap(), remote_bytes);

        // Local file untouched.
        let local_after = std::fs::read(&fixture.local_path).unwrap();
        assert_eq!(local_after, local_before, "local file must not be modified by [s]");

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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
        let progress = ProgressLog::start("test");
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
        progress.finish("");

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
        let base_hash = content_hash(&local_bytes);

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "labels",
            "audit-hold",
            ObjectEntry {
                id: 42,
                url: Some(local_label.url.clone()),
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

    /// Scripted `k\n` ([k]eep local; restore on env) on a RemoteDelete
    /// label: the resolver must promote a restore item (push pipeline
    /// will POST because the lockfile entry was dropped) and leave the
    /// local file intact.
    #[tokio::test]
    async fn resolve_remote_deletes_keep_local_promotes_to_restore() {
        let mut fixture = setup_remote_delete_fixture();
        let catalog = catalog_with_labels(vec![]);
        let classified = classified_remote_delete();
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
                url: Some(remote_label.url.clone()),
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
            remote_hash: Some(content_hash(&remote_bytes)),
            base_hash: Some("base".to_string()),
        }];
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
        let base_combined =
            crate::state::hook_combined_hash(&local_json_with_newline, &Some(base_code));
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: 42,
                url: Some("https://x.invalid/api/v1/hooks/42".to_string()),
                modified_at: Some("2026-05-14T08:00:00Z".to_string()),
                content_hash: Some(base_combined.clone()),
                secrets_hash: None,
            },
        );

        let catalog = catalog_with_hooks(vec![remote_hook.clone()]);

        let (remote_json_full, remote_code) =
            crate::snapshot::hook::serialize_hook(&remote_hook).unwrap();
        let remote_combined =
            crate::state::hook_combined_hash(&remote_json_full, &remote_code);
        let local_combined = crate::state::hook_combined_hash(
            &local_json_with_newline,
            &Some("def local_edit():\n    return 2\n".to_string()),
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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
        let expected =
            crate::state::hook_combined_hash(&remote_json_full, &remote_code);
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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
            crate::snapshot::noise::canonicalize_for_hash(&json_before);
        let json_canon_after =
            crate::snapshot::noise::canonicalize_for_hash(&json_after);
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
        let base_combined =
            crate::state::hook_combined_hash(&local_json_with_newline, &Some(base_code));
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: 42,
                url: Some("https://x.invalid/api/v1/hooks/42".to_string()),
                modified_at: Some("2026-05-14T08:00:00Z".to_string()),
                content_hash: Some(base_combined.clone()),
                secrets_hash: None,
            },
        );

        let catalog = catalog_with_hooks(vec![remote_hook.clone()]);

        let (remote_json_full, remote_code) =
            crate::snapshot::hook::serialize_hook(&remote_hook).unwrap();
        let remote_combined =
            crate::state::hook_combined_hash(&remote_json_full, &remote_code);
        let local_combined = crate::state::hook_combined_hash(
            &local_json_with_newline,
            &Some("def local_edit():\n    return 2\n".to_string()),
        );
        let classified = vec![ClassifiedItem {
            kind: "hooks".to_string(),
            slug: slug.to_string(),
            class: SyncClass::BothDiverged,
            local_hash: Some(local_combined),
            remote_hash: Some(remote_combined),
            base_hash: Some(base_combined.clone()),
        }];

        (tmp, paths, lockfile, local_json_path, local_py_path, catalog, classified, base_combined)
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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
        let base_combined =
            crate::state::hook_combined_hash(&local_json_with_newline, &Some(base_code));
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            slug,
            ObjectEntry {
                id: 42,
                url: Some("https://x.invalid/api/v1/hooks/42".to_string()),
                modified_at: Some("2026-05-14T08:00:00Z".to_string()),
                content_hash: Some(base_combined.clone()),
                secrets_hash: None,
            },
        );

        let catalog = catalog_with_hooks(vec![remote_hook.clone()]);

        let (remote_json_full, remote_code) =
            crate::snapshot::hook::serialize_hook(&remote_hook).unwrap();
        let remote_combined =
            crate::state::hook_combined_hash(&remote_json_full, &remote_code);
        // Local has no .py → local hash = `hook_combined_hash(json, None)`.
        let local_combined =
            crate::state::hook_combined_hash(&local_json_with_newline, &None);
        let classified = vec![ClassifiedItem {
            kind: "hooks".to_string(),
            slug: slug.to_string(),
            class: SyncClass::BothDiverged,
            local_hash: Some(local_combined),
            remote_hash: Some(remote_combined),
            base_hash: Some(base_combined.clone()),
        }];

        (tmp, paths, lockfile, local_json_path, catalog, classified, base_combined)
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
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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

        let base_combined =
            crate::state::schema_combined_hash(&base_json_bytes, &base_formulas);
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "schemas",
            q_slug,
            ObjectEntry {
                id: schema_id,
                url: Some(schema_url.clone()),
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
        let remote_combined =
            crate::state::schema_combined_hash(&remote_json_bytes, &remote_formulas);

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
                url: Some("https://x.invalid/api/v1/workspaces/800".to_string()),
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
                url: Some(queue.url.clone()),
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
        let local_combined =
            crate::state::schema_combined_hash(&base_json_bytes, &local_formulas);

        let classified = vec![ClassifiedItem {
            kind: "schemas".to_string(),
            slug: q_slug.to_string(),
            class: SyncClass::BothDiverged,
            local_hash: Some(local_combined),
            remote_hash: Some(remote_combined),
            base_hash: Some(base_combined.clone()),
        }];

        (tmp, paths, lockfile, schema_path, formula_path, catalog, classified, base_combined)
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
        let (_tmp, paths, mut lockfile, _schema_path, formula_path, catalog, classified, base_combined) =
            setup_schema_formula_only_conflict();
        let client = RossumClient::new(
            "https://unused.invalid/api/v1".to_string(),
            "TEST".to_string(),
        )
        .unwrap();
        let progress = ProgressLog::start("test");

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
        progress.finish("");

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
}
