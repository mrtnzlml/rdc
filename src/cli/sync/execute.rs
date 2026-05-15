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
//! - **Pull-side (`RemoteEdit`, `RemoteCreate`)** — grouped by kind and
//!   handed off to the per-kind pull driver with a `(kind, slug)` subset
//!   filter.
//! - **Push-side (`LocalEdit`, `LocalCreate`)** — folded into a
//!   `ChangeList` via [`crate::cli::push::scan::change_list_from_classified`]
//!   and dispatched through the existing push pipeline. Items promoted
//!   from the conflict and remote-delete branches are merged into the
//!   same ChangeList so they take a single round-trip through the push
//!   driver.
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
                    local_path,
                    id: q.id,
                    url: Some(q.url.clone()),
                    modified_at: q.modified_at().map(|s| s.to_string()),
                    hash_strategy: HashStrategy::Flat,
                })
            }),
            "schemas" => {
                // schemas are keyed by queue slug in the lockfile. Look up
                // the schema via the queue → schema_by_queue_id map; the
                // prompt only displays the JSON portion (formulas are
                // sidecar files and are followed by `[k]`/`[r]` decisions
                // identical to the JSON's). Treat as flat for resolution.
                queue_by_slug.get(it.slug.as_str()).and_then(|(q, ws_slug)| {
                    let schema = catalog.schemas_by_queue_id.get(&q.id)?;
                    let (json_bytes, _formulas) =
                        crate::snapshot::schema::serialize_schema(schema).ok()?;
                    let local_path = ctx.paths.queue_dir(ws_slug, &it.slug).join("schema.json");
                    Some(ConflictRefs {
                        remote_bytes: json_bytes,
                        remote_code: None,
                        local_path,
                        id: schema.id,
                        url: Some(schema.url.clone()),
                        modified_at: schema.modified_at().map(|s| s.to_string()),
                        hash_strategy: HashStrategy::Flat,
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
                        local_path,
                        id: t.id,
                        url: Some(t.url.clone()),
                        modified_at: t.modified_at().map(|s| s.to_string()),
                        hash_strategy: HashStrategy::Flat,
                    })
                })
            }
            other => {
                progress.println(format!(
                    "warning: conflict resolver not yet wired for kind '{}' (slug '{}'); skipping",
                    other, it.slug,
                ));
                None
            }
        }) else {
            // No catalog entry / orphan / unwired kind — warn and move on.
            progress.println(format!(
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
    local_path: PathBuf,
    id: u64,
    url: Option<String>,
    modified_at: Option<String>,
    /// How to fold `(remote_bytes, remote_code)` into the lockfile
    /// `content_hash`. Flat kinds use `Flat` (just `content_hash`);
    /// `Hook` / `Rule` use their respective combined-hash helpers so the
    /// adapter sees `Clean` after the resolution.
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
}

impl HashStrategy {
    /// Compute the canonical lockfile hash for the resolved bytes.
    fn hash(self, json_bytes: &[u8], code: &Option<String>) -> String {
        match self {
            HashStrategy::Flat => content_hash(json_bytes),
            HashStrategy::Hook => crate::state::hook_combined_hash(json_bytes, code),
            HashStrategy::Rule => crate::state::rule_combined_hash(json_bytes, code),
        }
    }
}

/// Run the conflict resolution loop for one item: prompts (or applies
/// the non-tty fallback) and routes the outcome to writes + lockfile
/// updates + push-side promotions. Extracted so each kind's arm above
/// stays a thin lookup; behavior matches the inlined labels block this
/// replaced.
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
        local_path,
        id,
        url,
        modified_at,
        hash_strategy,
    } = refs;

    // For split-file kinds, the sidecar lives next to the JSON. For
    // hooks the extension depends on `config.runtime` (`.py` or `.js`);
    // for rules it is always `.py` (Python is the only valid trigger
    // language). The resolver writes the sidecar on `[r]` (adopt
    // remote) and the local hash on `[s]`/non-tty paths includes
    // whatever code currently sits on disk.
    let code_path = sidecar_path_for_conflict(&local_path, hash_strategy);

    if !interactive {
        // Non-TTY/--yes: fall back to legacy shadow-file behavior so the
        // run still completes without blocking on stdin. The local file
        // stays as-is and the lockfile is pinned to the prior base —
        // advancing it would let the conflict silently disappear on
        // subsequent runs. Defensive fallback: when there is no prior
        // base (a conflict without a base is unusual), use the local
        // hash so the lockfile still gets a sensible entry.
        let conflict_path = crate::paths::shadow_path_for(&local_path, env);
        write_atomic(&conflict_path, &remote_bytes)?;
        progress.println(format!(
            "warn: {} conflict: local preserved, remote at {} (lockfile base preserved; re-run to resolve)",
            local_path.display(),
            conflict_path.display(),
        ));
        let preserved_hash = match it.base_hash.as_deref() {
            Some(prior) => prior.to_string(),
            None => {
                let local_bytes = std::fs::read(&local_path)
                    .with_context(|| format!("reading {}", local_path.display()))?;
                let local_code = if matches!(
                    hash_strategy,
                    HashStrategy::Hook | HashStrategy::Rule
                ) && code_path.exists()
                {
                    std::fs::read_to_string(&code_path).ok()
                } else {
                    None
                };
                hash_strategy.hash(&local_bytes, &local_code)
            }
        };
        crate::cli::pull::common::record_object(
            ctx.lockfile,
            &it.kind,
            &it.slug,
            id,
            url,
            modified_at,
            Some(preserved_hash),
        );
        return Ok(());
    }

    let resolution = prompt_resolve(
        input,
        stderr_lock,
        idx_one_based,
        total,
        &local_path,
        &remote_bytes,
        env,
    )?;

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
            let remote_hash = hash_strategy.hash(&remote_bytes, &remote_code);
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                url,
                modified_at,
                Some(remote_hash),
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
            // Adopt the remote sidecar too (or delete a stale one when
            // the remote has no code). Mirrors `pull::hooks`'s
            // `PullAction::Write` arm. For hooks we also recompute the
            // canonical sidecar path from the *remote* bytes, since the
            // remote runtime may have shifted between Python and
            // Node.js — sweep the now-stale opposite-extension file.
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
            }
            let remote_hash = hash_strategy.hash(&remote_bytes, &remote_code);
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                url,
                modified_at,
                Some(remote_hash),
            );
        }
        Resolution::Edit(edited) => {
            // Fully-resolved edit — write bytes to disk and align base to
            // remote so push drift detection succeeds, then promote the
            // item to the push pipeline.
            if let Some(parent) = local_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            write_atomic(&local_path, &edited)?;
            let remote_hash = hash_strategy.hash(&remote_bytes, &remote_code);
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                url,
                modified_at,
                Some(remote_hash),
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
            if let Some(parent) = local_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            write_atomic(&local_path, &edited)?;
            progress.println(format!(
                "warn: {} partially resolved (markers retained); lockfile base preserved; re-run to resolve",
                local_path.display(),
            ));
            let preserved_hash = match it.base_hash.as_deref() {
                Some(prior) => prior.to_string(),
                None => hash_strategy.hash(&edited, &remote_code),
            };
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                url,
                modified_at,
                Some(preserved_hash),
            );
        }
        Resolution::Skip => {
            // Shadow-file fallback. Write `<file>.<env>` with the remote
            // bytes, keep local on disk, pin the lockfile to the prior
            // base so subsequent runs re-prompt. Defensive fallback (no
            // prior base) uses the local hash to keep a sensible entry.
            let conflict_path = crate::paths::shadow_path_for(&local_path, env);
            write_atomic(&conflict_path, &remote_bytes)?;
            progress.println(format!(
                "warn: {} conflict: local preserved, remote at {} (lockfile base preserved; re-run to resolve)",
                local_path.display(),
                conflict_path.display(),
            ));
            let preserved_hash = match it.base_hash.as_deref() {
                Some(prior) => prior.to_string(),
                None => {
                    let local_bytes = std::fs::read(&local_path)
                        .with_context(|| format!("reading {}", local_path.display()))?;
                    let local_code = if matches!(
                        hash_strategy,
                        HashStrategy::Hook | HashStrategy::Rule
                    ) && code_path.exists()
                    {
                        std::fs::read_to_string(&code_path).ok()
                    } else {
                        None
                    };
                    hash_strategy.hash(&local_bytes, &local_code)
                }
            };
            crate::cli::pull::common::record_object(
                ctx.lockfile,
                &it.kind,
                &it.slug,
                id,
                url,
                modified_at,
                Some(preserved_hash),
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
                    progress.println(format!(
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
                        progress.println(format!(
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
                            progress.println(format!(
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
                        progress.println(format!(
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
                    progress.println(format!(
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
                            progress.println(format!(
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
                        progress.println(format!(
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
}
