//! Within-env slug realignment.
//!
//! Invoked via `rdc doctor <env>`. Walks the
//! lockfile, reads each object's local JSON `name` field, slugifies it,
//! and proposes a rename for every entry whose current sticky slug
//! differs from the proposed one.
//!
//! Pull never moves files. This is the explicit user-driven action that
//! brings stale local slugs into alignment with current remote names.
//!
//! Cascades:
//!
//! - A workspace rename moves the entire `workspaces/<old>/` dir to
//!   `workspaces/<new>/`. The lockfile's `workspaces.<old>` key moves
//!   to `<new>`. Every `email_templates` entry whose compound key
//!   starts with `<old>/` has its first segment rewritten.
//! - A queue rename moves `workspaces/<ws>/queues/<old>/` to
//!   `workspaces/<ws>/queues/<new>/` (one OS call brings schema.json,
//!   inbox.json, formulas/, and email-templates/ along). The lockfile
//!   keys `queues.<old>`, `schemas.<old>`, `inboxes.<old>` all move to
//!   `<new>`. Every `email_templates` compound key whose middle
//!   segment matches `<old>` is rewritten.
//!
//! Order of application: workspaces first, then queues, then leaves
//! (hooks/rules/labels/engines/engine_fields/workflows/workflow_steps
//! /email_templates). Between renames, pending entries are updated to
//! reflect cascades that just happened (so a queue under a renamed
//! workspace sees the new `ws_slug`).
//!
//! Overlay (`overlay.toml`) and mapping files (`.rdc/map/*.toml`) that
//! reference old slugs become orphans. We warn but do not modify those
//! files — they are user-authored configs.

use crate::paths::Paths;
use crate::slug::slugify;
use crate::state::Lockfile;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal, Write};

/// A single rename the user can choose to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingRename {
    Workspace {
        old: String,
        new: String,
    },
    Queue {
        ws: String,
        old: String,
        new: String,
    },
    EmailTemplate {
        ws: String,
        q: String,
        old: String,
        new: String,
    },
    Hook {
        old: String,
        new: String,
    },
    Rule {
        old: String,
        new: String,
    },
    Label {
        old: String,
        new: String,
    },
    Engine {
        old: String,
        new: String,
    },
    EngineField {
        old: String,
        new: String,
    },
    Workflow {
        old: String,
        new: String,
    },
    WorkflowStep {
        old: String,
        new: String,
    },
}

impl PendingRename {
    /// One-line summary for the interactive prompt and `--check` listing.
    pub fn describe(&self) -> String {
        match self {
            PendingRename::Workspace { old, new } => format!("workspaces/{old} -> {new}"),
            PendingRename::Queue { ws, old, new } => format!("queues/{ws}/{old} -> {new}"),
            PendingRename::EmailTemplate { ws, q, old, new } => {
                format!("email_templates/{ws}/{q}/{old} -> {new}")
            }
            PendingRename::Hook { old, new } => format!("hooks/{old} -> {new}"),
            PendingRename::Rule { old, new } => format!("rules/{old} -> {new}"),
            PendingRename::Label { old, new } => format!("labels/{old} -> {new}"),
            PendingRename::Engine { old, new } => format!("engines/{old} -> {new}"),
            PendingRename::EngineField { old, new } => format!("engine_fields/{old} -> {new}"),
            PendingRename::Workflow { old, new } => format!("workflows/{old} -> {new}"),
            PendingRename::WorkflowStep { old, new } => format!("workflow_steps/{old} -> {new}"),
        }
    }
}

/// Sort key so cascade-parent renames apply before their children:
/// workspaces and engines and workflows (each owns a subtree on disk)
/// before queues, queues before leaf objects.
fn priority(p: &PendingRename) -> u8 {
    match p {
        PendingRename::Workspace { .. } => 0,
        PendingRename::Engine { .. } => 0,
        PendingRename::Workflow { .. } => 0,
        PendingRename::Queue { .. } => 1,
        _ => 2,
    }
}

/// Walk the lockfile + local JSON files; return every rename that
/// would bring a stale slug into alignment.
///
/// Cheap when there's nothing to rename: one JSON read per entry, plus
/// `slugify` on the `name` field.
pub fn detect(paths: &Paths, lockfile: &Lockfile) -> Vec<PendingRename> {
    let mut out: Vec<PendingRename> = Vec::new();

    if let Some(ws_map) = lockfile.objects.get("workspaces") {
        for slug in ws_map.keys() {
            if let Some(name) = read_name(&paths.workspace_dir(slug).join("workspace.json")) {
                let proposed = slugify(&name);
                if proposed != *slug && !ws_map.contains_key(&proposed) {
                    out.push(PendingRename::Workspace {
                        old: slug.clone(),
                        new: proposed,
                    });
                }
            }
        }
    }

    if let Some(q_map) = lockfile.objects.get("queues") {
        for slug in q_map.keys() {
            if let Some((ws, q_path)) = locate_queue_dir(paths, lockfile, slug)
                && let Some(name) = read_name(&q_path.join("queue.json"))
            {
                let proposed = slugify(&name);
                if proposed != *slug && !q_map.contains_key(&proposed) {
                    out.push(PendingRename::Queue {
                        ws,
                        old: slug.clone(),
                        new: proposed,
                    });
                }
            }
        }
    }

    detect_flat_kind(lockfile, "hooks", paths.hooks_dir(), &mut out, |o, n| {
        PendingRename::Hook { old: o, new: n }
    });
    detect_flat_kind(lockfile, "rules", paths.rules_dir(), &mut out, |o, n| {
        PendingRename::Rule { old: o, new: n }
    });
    detect_flat_kind(lockfile, "labels", paths.labels_dir(), &mut out, |o, n| {
        PendingRename::Label { old: o, new: n }
    });
    detect_engines(paths, lockfile, &mut out);
    detect_engine_fields(paths, lockfile, &mut out);
    detect_workflows(paths, lockfile, &mut out);
    detect_workflow_steps(paths, lockfile, &mut out);

    if let Some(t_map) = lockfile.objects.get("email_templates") {
        for compound in t_map.keys() {
            let Some((ws, q, t)) = split_compound(compound) else {
                continue;
            };
            let tpl_path = paths
                .queue_email_templates_dir(&ws, &q)
                .join(format!("{t}.json"));
            let Some(name) = read_name(&tpl_path) else {
                continue;
            };
            let proposed = slugify(&name);
            if proposed != t {
                let new_compound = format!("{ws}/{q}/{proposed}");
                if !t_map.contains_key(&new_compound) {
                    out.push(PendingRename::EmailTemplate {
                        ws,
                        q,
                        old: t,
                        new: proposed,
                    });
                }
            }
        }
    }

    out.sort_by_key(priority);
    out
}

fn detect_flat_kind(
    lockfile: &Lockfile,
    kind: &str,
    dir: std::path::PathBuf,
    out: &mut Vec<PendingRename>,
    make: impl Fn(String, String) -> PendingRename,
) {
    let Some(by_slug) = lockfile.objects.get(kind) else {
        return;
    };
    for slug in by_slug.keys() {
        let file = dir.join(format!("{slug}.json"));
        let Some(name) = read_name(&file) else {
            continue;
        };
        let proposed = slugify(&name);
        if proposed != *slug && !by_slug.contains_key(&proposed) {
            out.push(make(slug.clone(), proposed));
        }
    }
}

/// Read a JSON file and return its top-level `name` field as a String.
/// Returns None for any IO/parse failure or a non-string `name`.
fn read_name(path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    v.get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
}

/// Detect stale engine slugs: read `engines/<slug>/engine.json` and
/// propose a rename if the JSON's `name` slugifies to something
/// different than `<slug>`. Engines own a directory on disk, so a
/// rename moves the whole subtree (engine.json + fields/).
fn detect_engines(paths: &Paths, lockfile: &Lockfile, out: &mut Vec<PendingRename>) {
    let Some(by_slug) = lockfile.objects.get("engines") else {
        return;
    };
    for slug in by_slug.keys() {
        let file = paths.engine_dir(slug).join("engine.json");
        let Some(name) = read_name(&file) else {
            continue;
        };
        let proposed = slugify(&name);
        if proposed != *slug && !by_slug.contains_key(&proposed) {
            out.push(PendingRename::Engine {
                old: slug.clone(),
                new: proposed,
            });
        }
    }
}

/// Detect stale engine-field slugs. Each field nests under exactly one
/// engine; lockfile keys are composite `<engine>/<field>`, so we slugify
/// just the field portion and recompose with the unchanged engine prefix.
fn detect_engine_fields(paths: &Paths, lockfile: &Lockfile, out: &mut Vec<PendingRename>) {
    let Some(by_slug) = lockfile.objects.get("engine_fields") else {
        return;
    };
    for key in by_slug.keys() {
        let Some(file) = locate_engine_field_file(paths, key) else {
            continue;
        };
        let Some(name) = read_name(&file) else {
            continue;
        };
        let proposed_field = slugify(&name);
        let proposed_key = match key.split_once('/') {
            Some((engine, _)) => format!("{engine}/{proposed_field}"),
            None => proposed_field.clone(),
        };
        if proposed_key != *key && !by_slug.contains_key(&proposed_key) {
            out.push(PendingRename::EngineField {
                old: key.clone(),
                new: proposed_key,
            });
        }
    }
}

/// Detect stale workflow slugs. Same pattern as engines.
fn detect_workflows(paths: &Paths, lockfile: &Lockfile, out: &mut Vec<PendingRename>) {
    let Some(by_slug) = lockfile.objects.get("workflows") else {
        return;
    };
    for slug in by_slug.keys() {
        let file = paths.workflow_dir(slug).join("workflow.json");
        let Some(name) = read_name(&file) else {
            continue;
        };
        let proposed = slugify(&name);
        if proposed != *slug && !by_slug.contains_key(&proposed) {
            out.push(PendingRename::Workflow {
                old: slug.clone(),
                new: proposed,
            });
        }
    }
}

/// Detect stale workflow-step slugs. Same composite-key pattern as
/// engine_fields: lockfile keys are `<workflow>/<step>`.
fn detect_workflow_steps(paths: &Paths, lockfile: &Lockfile, out: &mut Vec<PendingRename>) {
    let Some(by_slug) = lockfile.objects.get("workflow_steps") else {
        return;
    };
    for key in by_slug.keys() {
        let Some(file) = locate_workflow_step_file(paths, key) else {
            continue;
        };
        let Some(name) = read_name(&file) else {
            continue;
        };
        let proposed_step = slugify(&name);
        let proposed_key = match key.split_once('/') {
            Some((wf, _)) => format!("{wf}/{proposed_step}"),
            None => proposed_step.clone(),
        };
        if proposed_key != *key && !by_slug.contains_key(&proposed_key) {
            out.push(PendingRename::WorkflowStep {
                old: key.clone(),
                new: proposed_key,
            });
        }
    }
}

/// Resolve an engine-field on-disk path from its composite
/// `<engine_slug>/<field_slug>` lockfile key. Falls back to a global
/// walk for legacy flat keys that haven't migrated yet.
fn locate_engine_field_file(paths: &Paths, key: &str) -> Option<std::path::PathBuf> {
    if let Some((e_slug, f_slug)) = key.split_once('/') {
        let candidate = paths
            .engine_fields_dir(e_slug)
            .join(format!("{f_slug}.json"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let engines_dir = paths.engines_dir();
    let entries = std::fs::read_dir(&engines_dir).ok()?;
    for e_entry in entries.flatten() {
        if !e_entry.file_type().ok()?.is_dir() {
            continue;
        }
        let e_slug = e_entry.file_name().to_string_lossy().to_string();
        let p = paths.engine_fields_dir(&e_slug).join(format!("{key}.json"));
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Resolve a workflow-step on-disk path from its composite
/// `<workflow_slug>/<step_slug>` lockfile key. Legacy-flat fallback
/// mirrors `locate_engine_field_file`.
fn locate_workflow_step_file(paths: &Paths, key: &str) -> Option<std::path::PathBuf> {
    if let Some((w_slug, s_slug)) = key.split_once('/') {
        let candidate = paths
            .workflow_steps_dir(w_slug)
            .join(format!("{s_slug}.json"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let workflows_dir = paths.workflows_dir();
    let entries = std::fs::read_dir(&workflows_dir).ok()?;
    for w_entry in entries.flatten() {
        if !w_entry.file_type().ok()?.is_dir() {
            continue;
        }
        let w_slug = w_entry.file_name().to_string_lossy().to_string();
        let p = paths
            .workflow_steps_dir(&w_slug)
            .join(format!("{key}.json"));
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Locate the queue dir for a given q_slug by reading the queue.json's
/// `workspace` URL and looking up the parent ws_slug via the lockfile.
/// Returns (ws_slug, queue_dir_path).
fn locate_queue_dir(
    paths: &Paths,
    lockfile: &Lockfile,
    q_slug: &str,
) -> Option<(String, std::path::PathBuf)> {
    let ws_root = paths.workspaces_dir();
    let entries = std::fs::read_dir(&ws_root).ok()?;
    for entry in entries.flatten() {
        let ws_slug = entry.file_name().to_string_lossy().to_string();
        let candidate = entry.path().join("queues").join(q_slug);
        if candidate.is_dir() {
            // Sanity: parent ws_slug should exist in lockfile.
            if lockfile
                .objects
                .get("workspaces")
                .is_some_and(|m| m.contains_key(&ws_slug))
            {
                return Some((ws_slug, candidate));
            }
        }
    }
    None
}

fn split_compound(key: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = key.splitn(3, '/').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((
        parts[0].to_string(),
        parts[1].to_string(),
        parts[2].to_string(),
    ))
}

/// Stats produced by `apply`.
#[derive(Debug, Default)]
pub struct ApplyStats {
    pub applied: usize,
    pub skipped: usize,
    pub orphan_warnings: Vec<String>,
}

/// Apply the pending list. Mutates lockfile in place; caller is
/// responsible for persisting the result via `lockfile.save()`.
///
/// `interactive` controls whether each rename gets a `[y/N]` prompt
/// (TTY mode). When false, every rename is applied.
pub fn apply(
    paths: &Paths,
    lockfile: &mut Lockfile,
    mut pending: Vec<PendingRename>,
    interactive: bool,
) -> Result<ApplyStats> {
    let mut stats = ApplyStats::default();
    let mut idx = 0usize;
    let total = pending.len();
    // `rdc://<kind>/<old>` → `rdc://<kind>/<new>` for every applied rename.
    // Swept across the whole snapshot after all moves complete so the renamed
    // object's own `url` and every sibling's reference follow the new slug.
    let mut ref_subst: Vec<(String, String)> = Vec::new();
    // Compound own-url prefixes (`rdc://email_templates/<ws>/<q>/…`) whose
    // ws/q segment embeds a renamed slug. These are mid-string, not whole
    // tokens, so they need a separate prefix rewrite.
    let mut prefix_subst: Vec<(String, String)> = Vec::new();

    // Process in priority order: workspaces, queues, leaves. The list
    // is already sorted by `detect`.
    while idx < pending.len() {
        let p = pending[idx].clone();

        let confirmed = if interactive {
            prompt_rename(&p, idx + 1, total)?
        } else {
            true
        };

        if !confirmed {
            stats.skipped += 1;
            idx += 1;
            continue;
        }

        match apply_one(paths, lockfile, &p) {
            Ok(orphan_msgs) => {
                stats.applied += 1;
                stats.orphan_warnings.extend(orphan_msgs);
                ref_subst.extend(ref_subst_pairs(&p));
                prefix_subst.extend(compound_prefix_pairs(&p));
                // Cascade in-memory pending updates: if we just renamed
                // a workspace, later Queue / EmailTemplate entries
                // referencing the old ws_slug need their ws_slug
                // updated. Same for queue → email_template middle.
                cascade_pending(&mut pending[idx + 1..], &p);
            }
            Err(e) => {
                eprintln!("error applying {}: {e:#}", p.describe());
                stats.skipped += 1;
            }
        }
        idx += 1;
    }

    if !ref_subst.is_empty() || !prefix_subst.is_empty() {
        rewrite_refs_in_tree(paths, &ref_subst, &prefix_subst)?;
        refresh_lockfile_hashes(paths, lockfile)?;
    }

    Ok(stats)
}

/// Compound own-url prefix substitutions a rename implies. An email template's
/// own url is `rdc://email_templates/<ws>/<q>/<t>`, embedding both the
/// workspace and queue slug. When either renames, that mid-string segment must
/// follow — the whole-token sweep can't (the slug isn't the whole token). The
/// trailing `/` bounds the match so a slug never matches a longer sibling
/// (`…/cost/` never matches `…/cost-2/`). Engine-field / workflow-step compound
/// urls embed their parent slug the same way; those kinds aren't exercised here
/// so are left for a follow-up.
fn compound_prefix_pairs(p: &PendingRename) -> Vec<(String, String)> {
    let scheme = crate::snapshot::refs::RDC_SCHEME;
    let et = format!("{scheme}email_templates/");
    match p {
        PendingRename::Queue { ws, old, new } => {
            vec![(format!("{et}{ws}/{old}/"), format!("{et}{ws}/{new}/"))]
        }
        PendingRename::Workspace { old, new } => {
            vec![(format!("{et}{old}/"), format!("{et}{new}/"))]
        }
        PendingRename::Engine { old, new } => {
            let ef = format!("{scheme}engine_fields/");
            vec![(format!("{ef}{old}/"), format!("{ef}{new}/"))]
        }
        PendingRename::Workflow { old, new } => {
            let ws = format!("{scheme}workflow_steps/");
            vec![(format!("{ws}{old}/"), format!("{ws}{new}/"))]
        }
        _ => Vec::new(),
    }
}

/// Recompute every object's lockfile `content_hash` from its base-cache bytes.
///
/// The stored hash is slug-encoded: `canonicalize_for_hash` portabilizes a
/// body's references (URL → `rdc://<kind>/<slug>`) via the lockfile *before*
/// hashing. A slug rename changes that id→slug mapping, so the stored hash of
/// every object that references a renamed object silently goes stale — even
/// though the base-cache bytes never changed. Leaving it stale makes the next
/// sync see local≠base for each such object: the "both diverged" prompt storm.
///
/// The base cache holds the pulled body in canonical disk form (code/formulas
/// split into sidecars, server-noise stripped) with references still in URL
/// form. `combined_hash` runs `canonicalize_for_hash`, which portabilizes those
/// URLs via the *current* lockfile — so recomputing it after the rename
/// reproduces exactly the hash the next sync derives for the (unchanged) remote
/// under the new slug mapping. Objects referencing no renamed object hash
/// identically (idempotent no-op); the rest are corrected. Base bytes are the
/// right source — they are the last-synced remote, so an object whose `name`
/// the user *also* edited locally keeps its pending push (local≠base) rather
/// than being silently reverted. The renamed object's own base file was moved
/// to the new-slug path by `move_base_mirror`, so it classifies under the new
/// slug here. Best-effort per object: an unclassifiable path, unreadable file,
/// or unregistered kind is skipped rather than aborting the realign.
fn refresh_lockfile_hashes(paths: &Paths, lockfile: &mut Lockfile) -> Result<()> {
    let base_root = paths.base_cache_root();
    for base_path in json_files_under(&base_root) {
        let Ok(rel) = base_path.strip_prefix(&base_root) else {
            continue;
        };
        let Some((kind, slug)) = crate::cli::migrate::classify(rel) else {
            continue;
        };
        let Ok(json) = std::fs::read(&base_path) else {
            continue;
        };
        let sidecars = base_sidecars(kind, &base_path);
        let hash = crate::snapshot::codec::combined_hash(&json, &sidecars, lockfile);
        if let Some(entry) = lockfile
            .objects
            .get_mut(kind)
            .and_then(|m| m.get_mut(&slug))
        {
            entry.content_hash = Some(hash);
        }
    }
    Ok(())
}

/// Read a base-cache object's sidecar `(label, bytes)` pairs, matching the
/// labels each `KindCodec::disk_bytes` emits so the recomputed `combined_hash`
/// is byte-identical to the one pull recorded. The base cache mirrors the env
/// tree, so sidecars sit beside the JSON exactly as on disk. Code/formulas
/// carry no refs, so the slug rename leaves them untouched — but they must
/// still fold into the combined hash.
fn base_sidecars(kind: &str, base_json_path: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let stem = base_json_path.file_stem().and_then(|s| s.to_str());
    let dir = base_json_path.parent();
    match (kind, stem, dir) {
        ("hooks", Some(stem), Some(dir)) => {
            for ext in ["py", "js"] {
                if let Ok(bytes) = std::fs::read(dir.join(format!("{stem}.{ext}"))) {
                    return vec![("code".to_string(), bytes)];
                }
            }
            Vec::new()
        }
        ("rules", Some(stem), Some(dir)) => match std::fs::read(dir.join(format!("{stem}.py"))) {
            Ok(bytes) => vec![("trigger_condition".to_string(), bytes)],
            Err(_) => Vec::new(),
        },
        ("schemas", _, Some(queue_dir)) => {
            crate::snapshot::schema::read_local_formulas(queue_dir).unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

/// The `rdc://<kind>/<old>` → `rdc://<kind>/<new>` reference substitutions a
/// single applied rename implies. Emitted for every kind that other objects
/// reference by a whole-token `rdc://<kind>/<slug>` value: workspaces, queues
/// (which also drag their schema/inbox — those share the queue slug), hooks,
/// rules, labels, engines, and workflows (queues' `workflows[]` and steps'
/// `workflow` field point at them). This also rewrites the renamed object's
/// own `url`. The compound-keyed leaf kinds nothing references by a whole
/// token (engine_fields / email_templates / workflow_steps) instead get a
/// prefix rewrite of their own compound url via `compound_prefix_pairs`.
fn ref_subst_pairs(p: &PendingRename) -> Vec<(String, String)> {
    let pair = |kind: &str, old: &str, new: &str| {
        (
            format!("{}{kind}/{old}", crate::snapshot::refs::RDC_SCHEME),
            format!("{}{kind}/{new}", crate::snapshot::refs::RDC_SCHEME),
        )
    };
    match p {
        PendingRename::Workspace { old, new } => vec![pair("workspaces", old, new)],
        PendingRename::Queue { old, new, .. } => vec![
            pair("queues", old, new),
            pair("schemas", old, new),
            pair("inboxes", old, new),
        ],
        PendingRename::Hook { old, new } => vec![pair("hooks", old, new)],
        PendingRename::Rule { old, new } => vec![pair("rules", old, new)],
        PendingRename::Label { old, new } => vec![pair("labels", old, new)],
        PendingRename::Engine { old, new } => vec![pair("engines", old, new)],
        PendingRename::Workflow { old, new } => vec![pair("workflows", old, new)],
        PendingRename::EngineField { .. }
        | PendingRename::WorkflowStep { .. }
        | PendingRename::EmailTemplate { .. } => Vec::new(),
    }
}

/// Surgically rewrite portable refs across every `.json` file under both the
/// env tree AND the base-cache tree. A ref is always a complete,
/// quote-delimited JSON string value (`"rdc://<kind>/<slug>"`), never a
/// substring of a template or formula, so a byte-level replacement of the
/// quoted token is exact and — unlike a parse/re-serialize round-trip —
/// introduces zero key-order or formatting churn. The trailing quote also
/// prevents a slug from matching a longer slug that shares its prefix
/// (`…/cost"` never matches `…/cost-invoices"`).
///
/// The base-cache sweep is essential: the base mirrors the *portabilized*
/// remote, whose slugs follow the lockfile. After a slug rename, every
/// referencing object's base still carries the old ref; leaving it stale makes
/// the next sync see local≠base even when local==remote, raising a spurious
/// "both diverged" conflict for every referencing object.
/// Sweeps both the env tree (always `rdc://`-form) and the base-cache tree
/// (raw remote, usually URL-form but `rdc://` for some pull paths). The base
/// sweep is a no-op on URL-form bodies; the lockfile-hash refresh handles the
/// URL→slug portabilization shift for those. Both passes together leave the
/// snapshot's reference graph consistent regardless of stored form.
fn rewrite_refs_in_tree(
    paths: &Paths,
    subst: &[(String, String)],
    prefix_subst: &[(String, String)],
) -> Result<()> {
    // Whole-token refs are quote-delimited (`"rdc://kind/slug"`); prefix refs
    // are mid-string segments of a compound url, bounded by a trailing `/`.
    let needles: Vec<(String, String)> = subst
        .iter()
        .map(|(old, new)| (format!("\"{old}\""), format!("\"{new}\"")))
        .collect();
    for root in [paths.env_root(), paths.base_cache_root()] {
        for path in json_files_under(&root) {
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut updated = raw.clone();
            for (old, new) in &needles {
                if updated.contains(old.as_str()) {
                    updated = updated.replace(old.as_str(), new.as_str());
                }
            }
            for (old, new) in prefix_subst {
                if updated.contains(old.as_str()) {
                    updated = updated.replace(old.as_str(), new.as_str());
                }
            }
            if updated != raw {
                crate::snapshot::writer::write_atomic(&path, updated.as_bytes())?;
            }
        }
    }
    Ok(())
}

/// Recursively collect every `*.json` file under `root` (skips missing roots).
fn json_files_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("json") {
                out.push(path);
            }
        }
    }
    out
}

fn cascade_pending(rest: &mut [PendingRename], applied: &PendingRename) {
    match applied {
        PendingRename::Workspace { old, new } => {
            for p in rest.iter_mut() {
                match p {
                    PendingRename::Queue { ws, .. } if ws == old => *ws = new.clone(),
                    PendingRename::EmailTemplate { ws, .. } if ws == old => {
                        *ws = new.clone();
                    }
                    _ => {}
                }
            }
        }
        PendingRename::Queue { old, new, .. } => {
            for p in rest.iter_mut() {
                if let PendingRename::EmailTemplate { q, .. } = p
                    && q == old
                {
                    *q = new.clone();
                }
            }
        }
        _ => {}
    }
}

fn prompt_rename(p: &PendingRename, n: usize, total: usize) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        // No-TTY path shouldn't reach here (caller passes interactive=false),
        // but if it does, default to skip.
        return Ok(false);
    }
    let stdin = std::io::stdin();
    let mut stderr = std::io::stderr();
    let mut line = String::new();
    loop {
        write!(stderr, "[{n}/{total}] apply {}? [y/N] ", p.describe())?;
        stderr.flush().ok();
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(false); // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(false);
        }
        match trimmed.chars().next() {
            Some('y') | Some('Y') => return Ok(true),
            Some('n') | Some('N') => return Ok(false),
            _ => {
                writeln!(stderr, "  please answer y or n")?;
            }
        }
    }
}

fn apply_one(paths: &Paths, lockfile: &mut Lockfile, p: &PendingRename) -> Result<Vec<String>> {
    let mut orphans: Vec<String> = Vec::new();
    match p {
        PendingRename::Hook { old, new } => {
            move_file(
                paths,
                &paths.hooks_dir().join(format!("{old}.json")),
                &paths.hooks_dir().join(format!("{new}.json")),
            )?;
            // Sidecar may be `.py` (Python) or `.js` (Node.js) — move
            // whichever happens to exist. Both extensions are a no-op
            // when absent thanks to `move_optional`.
            move_optional(
                paths,
                &paths.hooks_dir().join(format!("{old}.py")),
                &paths.hooks_dir().join(format!("{new}.py")),
            )?;
            move_optional(
                paths,
                &paths.hooks_dir().join(format!("{old}.js")),
                &paths.hooks_dir().join(format!("{new}.js")),
            )?;
            rename_lockfile_key(lockfile, "hooks", old, new);
            collect_orphans(paths, "hooks", old, &mut orphans);
        }
        PendingRename::Rule { old, new } => {
            // Rules are a combined-form kind (json + optional
            // trigger_condition .py). Move both files when present.
            move_file(
                paths,
                &paths.rules_dir().join(format!("{old}.json")),
                &paths.rules_dir().join(format!("{new}.json")),
            )?;
            move_optional(
                paths,
                &paths.rules_dir().join(format!("{old}.py")),
                &paths.rules_dir().join(format!("{new}.py")),
            )?;
            rename_lockfile_key(lockfile, "rules", old, new);
            collect_orphans(paths, "rules", old, &mut orphans);
        }
        PendingRename::Label { old, new } => {
            move_file(
                paths,
                &paths.labels_dir().join(format!("{old}.json")),
                &paths.labels_dir().join(format!("{new}.json")),
            )?;
            rename_lockfile_key(lockfile, "labels", old, new);
            collect_orphans(paths, "labels", old, &mut orphans);
        }
        PendingRename::Engine { old, new } => {
            // Engines own a dir (engine.json + fields/); a rename moves
            // the whole subtree. The field slugs are unchanged, but their
            // compound lockfile keys `<engine>/<field>` embed the engine
            // slug, so the engine segment must cascade.
            move_dir(paths, &paths.engine_dir(old), &paths.engine_dir(new))?;
            rename_lockfile_key(lockfile, "engines", old, new);
            rewrite_compound_prefix(lockfile, "engine_fields", 0, old, new);
            collect_orphans(paths, "engines", old, &mut orphans);
        }
        PendingRename::EngineField { old, new } => {
            // `old` / `new` are composite `<engine>/<field>` lockfile
            // keys; the on-disk filename uses just the field portion.
            if let Some(old_path) = locate_engine_field_file(paths, old)
                && let Some(parent) = old_path.parent()
            {
                let new_field = new.split_once('/').map(|(_, f)| f).unwrap_or(new);
                move_file(paths, &old_path, &parent.join(format!("{new_field}.json")))?;
            }
            rename_lockfile_key(lockfile, "engine_fields", old, new);
            collect_orphans(paths, "engine_fields", old, &mut orphans);
        }
        PendingRename::Workflow { old, new } => {
            // Workflows own a dir (workflow.json + steps/); same shape
            // as engines — the step keys `<workflow>/<step>` embed the
            // workflow slug, so its segment must cascade.
            move_dir(paths, &paths.workflow_dir(old), &paths.workflow_dir(new))?;
            rename_lockfile_key(lockfile, "workflows", old, new);
            rewrite_compound_prefix(lockfile, "workflow_steps", 0, old, new);
            collect_orphans(paths, "workflows", old, &mut orphans);
        }
        PendingRename::WorkflowStep { old, new } => {
            if let Some(old_path) = locate_workflow_step_file(paths, old)
                && let Some(parent) = old_path.parent()
            {
                let new_step = new.split_once('/').map(|(_, s)| s).unwrap_or(new);
                move_file(paths, &old_path, &parent.join(format!("{new_step}.json")))?;
            }
            rename_lockfile_key(lockfile, "workflow_steps", old, new);
        }
        PendingRename::EmailTemplate { ws, q, old, new } => {
            let dir = paths.queue_email_templates_dir(ws, q);
            move_file(
                paths,
                &dir.join(format!("{old}.json")),
                &dir.join(format!("{new}.json")),
            )?;
            let old_compound = format!("{ws}/{q}/{old}");
            let new_compound = format!("{ws}/{q}/{new}");
            rename_lockfile_key(lockfile, "email_templates", &old_compound, &new_compound);
            collect_orphans(paths, "email_templates", &old_compound, &mut orphans);
        }
        PendingRename::Workspace { old, new } => {
            move_dir(paths, &paths.workspace_dir(old), &paths.workspace_dir(new))?;
            rename_lockfile_key(lockfile, "workspaces", old, new);
            rewrite_email_template_compound_prefix(lockfile, 0, old, new);
            collect_orphans(paths, "workspaces", old, &mut orphans);
        }
        PendingRename::Queue { ws, old, new } => {
            move_dir(paths, &paths.queue_dir(ws, old), &paths.queue_dir(ws, new))?;
            rename_lockfile_key(lockfile, "queues", old, new);
            rename_lockfile_key(lockfile, "schemas", old, new);
            rename_lockfile_key(lockfile, "inboxes", old, new);
            rewrite_email_template_compound_prefix(lockfile, 1, old, new);
            collect_orphans(paths, "queues", old, &mut orphans);
        }
    }
    Ok(orphans)
}

fn move_file(paths: &Paths, from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    if to.exists() {
        anyhow::bail!("destination {} already exists", to.display());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::rename(from, to)
        .with_context(|| format!("moving {} -> {}", from.display(), to.display()))?;
    move_base_mirror(paths, from, to)
}

fn move_optional(paths: &Paths, from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    move_file(paths, from, to)
}

fn move_dir(paths: &Paths, from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    if to.exists() {
        anyhow::bail!("destination {} already exists", to.display());
    }
    std::fs::rename(from, to)
        .with_context(|| format!("moving {} -> {}", from.display(), to.display()))?;
    move_base_mirror(paths, from, to)
}

/// Mirror an env-tree move onto the base-cache tree. The base cache mirrors
/// the env tree 1:1 (`state::base_cache`), so when a rename relocates an
/// object's files we must move the matching base sidecar(s) to the new slug —
/// otherwise the renamed object loses its 3-way-merge base and the next sync
/// reports a spurious "both diverged" conflict instead of a clean push.
/// Best-effort: a missing base mirror (never synced) or a path outside the env
/// tree is a no-op, not an error.
fn move_base_mirror(paths: &Paths, from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    let (Some(base_from), Some(base_to)) = (
        crate::state::base_cache::cache_mirror(paths, from),
        crate::state::base_cache::cache_mirror(paths, to),
    ) else {
        return Ok(());
    };
    if !base_from.exists() {
        return Ok(());
    }
    if let Some(parent) = base_to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::rename(&base_from, &base_to).with_context(|| {
        format!(
            "moving base cache {} -> {}",
            base_from.display(),
            base_to.display()
        )
    })
}

/// Move a slug-keyed entry within a lockfile kind.
fn rename_lockfile_key(lockfile: &mut Lockfile, kind: &str, old: &str, new: &str) {
    let Some(by_slug) = lockfile.objects.get_mut(kind) else {
        return;
    };
    if let Some(entry) = by_slug.remove(old) {
        by_slug.insert(new.to_string(), entry);
    }
}

/// Rewrite the Nth `/`-segment of every `email_templates` compound key
/// from `old` to `new`. `segment` is 0 (ws_slug) or 1 (q_slug).
fn rewrite_email_template_compound_prefix(
    lockfile: &mut Lockfile,
    segment: usize,
    old: &str,
    new: &str,
) {
    rewrite_compound_prefix(lockfile, "email_templates", segment, old, new);
}

/// Rewrite the Nth `/`-segment of every compound lockfile key of `kind` from
/// `old` to `new`. Used when a container renames and its children's keys embed
/// the container slug: `email_templates` (`<ws>/<q>/<t>`, segments 0/1),
/// `engine_fields` (`<engine>/<field>`, segment 0), `workflow_steps`
/// (`<workflow>/<step>`, segment 0). `splitn(3)` keeps the trailing segment
/// intact, so it handles both 2- and 3-segment keys.
fn rewrite_compound_prefix(
    lockfile: &mut Lockfile,
    kind: &str,
    segment: usize,
    old: &str,
    new: &str,
) {
    let Some(by_key) = lockfile.objects.get_mut(kind) else {
        return;
    };
    let to_rewrite: Vec<String> = by_key
        .keys()
        .filter(|k| {
            let parts: Vec<&str> = k.splitn(3, '/').collect();
            parts.get(segment).copied() == Some(old)
        })
        .cloned()
        .collect();
    for old_key in to_rewrite {
        let parts: Vec<&str> = old_key.splitn(3, '/').collect();
        let mut owned: Vec<String> = parts.iter().map(|s| s.to_string()).collect();
        owned[segment] = new.to_string();
        let new_key = owned.join("/");
        if let Some(entry) = by_key.remove(&old_key) {
            by_key.insert(new_key, entry);
        }
    }
}

/// Scan overlay.toml and any .rdc/map/*.toml file for textual
/// references to the old slug; return one warning string per file that
/// matches. We don't rewrite — the user is in control of those files.
fn collect_orphans(paths: &Paths, kind: &str, old: &str, out: &mut Vec<String>) {
    let needle = format!("\"{old}\"");
    let dotted = format!(".{old}]");
    let candidates: Vec<std::path::PathBuf> = std::iter::once(paths.overlay_file())
        .chain(list_mapping_files(paths))
        .collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for path in candidates {
        if !path.exists() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        if raw.contains(&needle) || raw.contains(&dotted) {
            let msg = format!(
                "  {} references {kind}/{old}; update manually",
                path.display()
            );
            if seen.insert(msg.clone()) {
                out.push(msg);
            }
        }
    }
}

fn list_mapping_files(paths: &Paths) -> Vec<std::path::PathBuf> {
    let dir = paths.mapping_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect()
}

/// Entry point used by `rdc doctor <env>`.
pub async fn run_within_env(env: &str, check: bool, yes: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);
    let cfg = crate::config::ProjectConfig::load(&paths.project_config())?;
    let env_cfg = cfg
        .envs
        .get(env)
        .ok_or_else(|| anyhow::anyhow!("env '{env}' is not defined in rdc.toml"))?;
    let mut lockfile = Lockfile::load(&paths.lockfile())?;
    // Derive object URLs from ids — set the env's api_base on the lockfile.
    lockfile.api_base = env_cfg.api_base.clone();

    let pending = detect(&paths, &lockfile);
    if pending.is_empty() {
        println!("env '{env}': no pending renames");
        return Ok(());
    }

    if check {
        println!("env '{env}' has {} pending renames:", pending.len());
        for p in &pending {
            println!("  {}", p.describe());
        }
        return Ok(());
    }

    let interactive = !yes && std::io::stdin().is_terminal() && std::io::stderr().is_terminal();

    let stats = apply(&paths, &mut lockfile, pending, interactive)?;

    if stats.applied > 0 {
        lockfile.save(&paths.lockfile())?;
    }

    println!(
        "env '{env}': {} renames applied, {} skipped",
        stats.applied, stats.skipped
    );
    if !stats.orphan_warnings.is_empty() {
        println!("note: overlay / mapping files reference renamed slugs:");
        for w in &stats.orphan_warnings {
            println!("{w}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_orders_workspaces_first() {
        let h = PendingRename::Hook {
            old: "a".into(),
            new: "b".into(),
        };
        let q = PendingRename::Queue {
            ws: "ws".into(),
            old: "a".into(),
            new: "b".into(),
        };
        let w = PendingRename::Workspace {
            old: "a".into(),
            new: "b".into(),
        };
        let mut v = vec![h.clone(), q.clone(), w.clone()];
        v.sort_by_key(priority);
        assert_eq!(v, vec![w, q, h]);
    }

    #[test]
    fn split_compound_basic() {
        let r = split_compound("ws/q/t").unwrap();
        assert_eq!(r, ("ws".to_string(), "q".to_string(), "t".to_string()));
    }

    #[test]
    fn split_compound_with_extra_slashes_keeps_last_two_segments() {
        // splitn(3) leaves the tail intact.
        let r = split_compound("ws/q/t-with/slash").unwrap();
        assert_eq!(r.2, "t-with/slash");
    }

    #[test]
    fn split_compound_rejects_two_segments() {
        assert!(split_compound("ws/q").is_none());
    }

    #[test]
    fn cascade_workspace_rename_propagates() {
        let mut rest = vec![
            PendingRename::Queue {
                ws: "old-ws".into(),
                old: "q1".into(),
                new: "q1-new".into(),
            },
            PendingRename::EmailTemplate {
                ws: "old-ws".into(),
                q: "q1".into(),
                old: "t1".into(),
                new: "t1-new".into(),
            },
            PendingRename::Hook {
                old: "h1".into(),
                new: "h1-new".into(),
            },
        ];
        let applied = PendingRename::Workspace {
            old: "old-ws".into(),
            new: "new-ws".into(),
        };
        cascade_pending(&mut rest, &applied);
        match &rest[0] {
            PendingRename::Queue { ws, .. } => assert_eq!(ws, "new-ws"),
            _ => panic!(),
        }
        match &rest[1] {
            PendingRename::EmailTemplate { ws, .. } => assert_eq!(ws, "new-ws"),
            _ => panic!(),
        }
    }

    #[test]
    fn cascade_queue_rename_propagates_to_email_templates() {
        let mut rest = vec![PendingRename::EmailTemplate {
            ws: "ws".into(),
            q: "q-old".into(),
            old: "t1".into(),
            new: "t1-new".into(),
        }];
        let applied = PendingRename::Queue {
            ws: "ws".into(),
            old: "q-old".into(),
            new: "q-new".into(),
        };
        cascade_pending(&mut rest, &applied);
        match &rest[0] {
            PendingRename::EmailTemplate { q, .. } => assert_eq!(q, "q-new"),
            _ => panic!(),
        }
    }

    #[test]
    fn rewrite_email_template_compound_prefix_ws_segment() {
        use crate::state::ObjectEntry;
        let mut lf = Lockfile::default();
        lf.upsert(
            "email_templates",
            "ws-old/q1/t1",
            ObjectEntry {
                id: 1,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lf.upsert(
            "email_templates",
            "ws-old/q2/t2",
            ObjectEntry {
                id: 2,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lf.upsert(
            "email_templates",
            "ws-other/q3/t3",
            ObjectEntry {
                id: 3,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        rewrite_email_template_compound_prefix(&mut lf, 0, "ws-old", "ws-new");
        let keys: Vec<&String> = lf.objects.get("email_templates").unwrap().keys().collect();
        assert!(keys.contains(&&"ws-new/q1/t1".to_string()));
        assert!(keys.contains(&&"ws-new/q2/t2".to_string()));
        assert!(keys.contains(&&"ws-other/q3/t3".to_string()));
        assert!(!keys.contains(&&"ws-old/q1/t1".to_string()));
    }

    #[test]
    fn rewrite_email_template_compound_prefix_q_segment() {
        use crate::state::ObjectEntry;
        let mut lf = Lockfile::default();
        lf.upsert(
            "email_templates",
            "ws/q-old/t1",
            ObjectEntry {
                id: 1,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lf.upsert(
            "email_templates",
            "ws/q-other/t2",
            ObjectEntry {
                id: 2,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        rewrite_email_template_compound_prefix(&mut lf, 1, "q-old", "q-new");
        let keys: Vec<&String> = lf.objects.get("email_templates").unwrap().keys().collect();
        assert!(keys.contains(&&"ws/q-new/t1".to_string()));
        assert!(keys.contains(&&"ws/q-other/t2".to_string()));
    }

    #[test]
    fn rename_lockfile_key_moves_entry() {
        use crate::state::ObjectEntry;
        let mut lf = Lockfile::default();
        let entry = ObjectEntry {
            id: 42,
            modified_at: None,
            content_hash: Some("abc".into()),
            secrets_hash: None,
        };
        lf.upsert("hooks", "old", entry.clone());
        rename_lockfile_key(&mut lf, "hooks", "old", "new");
        assert!(!lf.objects.get("hooks").unwrap().contains_key("old"));
        let got = lf.objects.get("hooks").unwrap().get("new").unwrap();
        assert_eq!(got.id, 42);
        assert_eq!(got.content_hash.as_deref(), Some("abc"));
    }

    #[test]
    fn describe_formats_each_kind() {
        let h = PendingRename::Hook {
            old: "a".into(),
            new: "b".into(),
        };
        assert_eq!(h.describe(), "hooks/a -> b");
        let q = PendingRename::Queue {
            ws: "ws".into(),
            old: "a".into(),
            new: "b".into(),
        };
        assert_eq!(q.describe(), "queues/ws/a -> b");
        let t = PendingRename::EmailTemplate {
            ws: "ws".into(),
            q: "q".into(),
            old: "t".into(),
            new: "t2".into(),
        };
        assert_eq!(t.describe(), "email_templates/ws/q/t -> t2");
    }

    /// Stage a hook on disk + in a lockfile, with a mismatched name, and
    /// apply: file moves, lockfile key moves, sidecar .py follows.
    #[test]
    fn apply_hook_rename_moves_files_and_lockfile() {
        use crate::state::ObjectEntry;
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();
        std::fs::write(
            paths.hooks_dir().join("validator-invoices.json"),
            r#"{"id":1,"name":"Validator Invoices v2","queues":[]}"#,
        )
        .unwrap();
        std::fs::write(
            paths.hooks_dir().join("validator-invoices.py"),
            b"def run(p): return p",
        )
        .unwrap();

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            "validator-invoices",
            ObjectEntry {
                id: 1,
                modified_at: None,
                content_hash: Some("h".into()),
                secrets_hash: None,
            },
        );

        let pending = detect(&paths, &lockfile);
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0],
            PendingRename::Hook {
                old: "validator-invoices".into(),
                new: "validator-invoices-v2".into(),
            }
        );

        let stats = apply(&paths, &mut lockfile, pending, false).unwrap();
        assert_eq!(stats.applied, 1);
        assert!(
            paths
                .hooks_dir()
                .join("validator-invoices-v2.json")
                .exists()
        );
        assert!(paths.hooks_dir().join("validator-invoices-v2.py").exists());
        assert!(!paths.hooks_dir().join("validator-invoices.json").exists());
        assert!(!paths.hooks_dir().join("validator-invoices.py").exists());
        assert!(
            lockfile
                .objects
                .get("hooks")
                .unwrap()
                .contains_key("validator-invoices-v2")
        );
        assert!(
            !lockfile
                .objects
                .get("hooks")
                .unwrap()
                .contains_key("validator-invoices")
        );
    }

    /// Workspace rename cascade: ws dir moves AND every email_template
    /// compound key gets its first segment rewritten.
    #[test]
    fn apply_workspace_rename_cascades_to_email_template_keys() {
        use crate::state::ObjectEntry;
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.workspace_dir("old-ws").join("queues/q1")).unwrap();
        std::fs::write(
            paths.workspace_dir("old-ws").join("workspace.json"),
            r#"{"id":1,"name":"New Workspace","queues":[]}"#,
        )
        .unwrap();

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "workspaces",
            "old-ws",
            ObjectEntry {
                id: 1,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lockfile.upsert(
            "email_templates",
            "old-ws/q1/t1",
            ObjectEntry {
                id: 10,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lockfile.upsert(
            "email_templates",
            "old-ws/q1/t2",
            ObjectEntry {
                id: 11,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lockfile.upsert(
            "email_templates",
            "other-ws/q9/t9",
            ObjectEntry {
                id: 99,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );

        let pending = detect(&paths, &lockfile);
        assert_eq!(pending.len(), 1);

        let stats = apply(&paths, &mut lockfile, pending, false).unwrap();
        assert_eq!(stats.applied, 1);
        assert!(paths.workspace_dir("new-workspace").exists());
        assert!(!paths.workspace_dir("old-ws").exists());
        let et_keys: Vec<&String> = lockfile
            .objects
            .get("email_templates")
            .unwrap()
            .keys()
            .collect();
        assert!(et_keys.contains(&&"new-workspace/q1/t1".to_string()));
        assert!(et_keys.contains(&&"new-workspace/q1/t2".to_string()));
        // Other-ws templates untouched.
        assert!(et_keys.contains(&&"other-ws/q9/t9".to_string()));
    }

    /// Queue rename: queue dir moves bringing schema.json/inbox.json/etc.
    /// along; queues/schemas/inboxes lockfile keys all move; email_template
    /// middle segments rewritten.
    #[test]
    fn apply_queue_rename_cascades_to_schema_inbox_and_email_templates() {
        use crate::state::ObjectEntry;
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let queue_dir = paths.queue_dir("ws1", "cost-invoices");
        std::fs::create_dir_all(&queue_dir).unwrap();
        std::fs::write(
            queue_dir.join("queue.json"),
            r#"{"id":1,"name":"AP Invoices","queues":[],"workspace":null,"schema":null}"#,
        )
        .unwrap();
        std::fs::write(queue_dir.join("schema.json"), b"{}").unwrap();
        std::fs::write(queue_dir.join("inbox.json"), b"{}").unwrap();
        // Need workspace dir for lockfile consistency check in detect.
        std::fs::create_dir_all(paths.workspace_dir("ws1")).unwrap();

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "workspaces",
            "ws1",
            ObjectEntry {
                id: 9,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lockfile.upsert(
            "queues",
            "cost-invoices",
            ObjectEntry {
                id: 1,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lockfile.upsert(
            "schemas",
            "cost-invoices",
            ObjectEntry {
                id: 2,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lockfile.upsert(
            "inboxes",
            "cost-invoices",
            ObjectEntry {
                id: 3,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lockfile.upsert(
            "email_templates",
            "ws1/cost-invoices/welcome",
            ObjectEntry {
                id: 4,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lockfile.upsert(
            "email_templates",
            "ws1/cost-invoices/rejected",
            ObjectEntry {
                id: 5,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );

        let pending = detect(&paths, &lockfile);
        let queue_pending: Vec<_> = pending
            .iter()
            .filter(|p| matches!(p, PendingRename::Queue { .. }))
            .collect();
        assert_eq!(queue_pending.len(), 1);

        let stats = apply(&paths, &mut lockfile, pending, false).unwrap();
        assert!(stats.applied >= 1);
        assert!(paths.queue_dir("ws1", "ap-invoices").exists());
        assert!(!paths.queue_dir("ws1", "cost-invoices").exists());
        // All three queue-keyed lockfile entries moved.
        assert!(
            lockfile
                .objects
                .get("queues")
                .unwrap()
                .contains_key("ap-invoices")
        );
        assert!(
            lockfile
                .objects
                .get("schemas")
                .unwrap()
                .contains_key("ap-invoices")
        );
        assert!(
            lockfile
                .objects
                .get("inboxes")
                .unwrap()
                .contains_key("ap-invoices")
        );
        // Old keys gone.
        assert!(
            !lockfile
                .objects
                .get("queues")
                .unwrap()
                .contains_key("cost-invoices")
        );
        assert!(
            !lockfile
                .objects
                .get("schemas")
                .unwrap()
                .contains_key("cost-invoices")
        );
        assert!(
            !lockfile
                .objects
                .get("inboxes")
                .unwrap()
                .contains_key("cost-invoices")
        );
        // Email_template compound middle segment rewritten.
        let et_keys: Vec<&String> = lockfile
            .objects
            .get("email_templates")
            .unwrap()
            .keys()
            .collect();
        assert!(et_keys.contains(&&"ws1/ap-invoices/welcome".to_string()));
        assert!(et_keys.contains(&&"ws1/ap-invoices/rejected".to_string()));
    }

    #[test]
    fn detect_skips_when_proposed_slug_already_taken() {
        use crate::state::ObjectEntry;
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();
        std::fs::write(
            paths.hooks_dir().join("hook-a.json"),
            r#"{"id":1,"name":"Hook B","queues":[]}"#,
        )
        .unwrap();
        // hook-b already exists in the lockfile (some other hook).
        std::fs::write(
            paths.hooks_dir().join("hook-b.json"),
            r#"{"id":2,"name":"Hook B","queues":[]}"#,
        )
        .unwrap();
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            "hook-a",
            ObjectEntry {
                id: 1,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        lockfile.upsert(
            "hooks",
            "hook-b",
            ObjectEntry {
                id: 2,
                modified_at: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        let pending = detect(&paths, &lockfile);
        assert!(
            pending.is_empty(),
            "expected no rename (would collide), got {pending:?}"
        );
    }

    /// Portable-refs gap #1: a rename must rewrite the renamed object's own
    /// `url` field AND every `rdc://<kind>/<old>` reference in sibling files,
    /// or the snapshot's reference graph dangles at the old slug.
    #[test]
    fn apply_hook_rename_rewrites_own_url_and_referencing_queue() {
        use crate::state::ObjectEntry;
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();
        std::fs::write(
            paths.hooks_dir().join("val.json"),
            r#"{"id":1,"name":"Validator V2","url":"rdc://hooks/val","queues":["rdc://queues/q1"]}"#,
        )
        .unwrap();
        let qdir = paths.queue_dir("ws1", "q1");
        std::fs::create_dir_all(&qdir).unwrap();
        std::fs::write(
            qdir.join("queue.json"),
            r#"{"id":2,"name":"Q1","url":"rdc://queues/q1","hooks":["rdc://hooks/val"]}"#,
        )
        .unwrap();

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            "val",
            ObjectEntry { id: 1, modified_at: None, content_hash: Some("h".into()), secrets_hash: None },
        );
        lockfile.upsert(
            "queues",
            "q1",
            ObjectEntry { id: 2, modified_at: None, content_hash: None, secrets_hash: None },
        );

        let pending = detect(&paths, &lockfile);
        assert_eq!(
            pending,
            vec![PendingRename::Hook { old: "val".into(), new: "validator-v2".into() }]
        );
        let stats = apply(&paths, &mut lockfile, pending, false).unwrap();
        assert_eq!(stats.applied, 1);

        let hook = std::fs::read_to_string(paths.hooks_dir().join("validator-v2.json")).unwrap();
        assert!(
            hook.contains(r#""rdc://hooks/validator-v2""#),
            "renamed hook's own url must be rewritten, got: {hook}"
        );
        let q = std::fs::read_to_string(qdir.join("queue.json")).unwrap();
        assert!(
            q.contains(r#""rdc://hooks/validator-v2""#),
            "referencing queue's hooks[] must be rewritten, got: {q}"
        );
        assert!(
            !q.contains(r#""rdc://hooks/val""#),
            "old slug ref must be gone, got: {q}"
        );
    }

    /// Portable-refs gap #2: a queue rename must rewrite `rdc://queues/<old>`
    /// AND `rdc://schemas/<old>` refs everywhere (queue/schema share the slug),
    /// across workspace.json, the schema, and every referencing hook.
    #[test]
    fn apply_queue_rename_rewrites_refs_across_workspace_schema_and_hooks() {
        use crate::state::ObjectEntry;
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let qdir = paths.queue_dir("ws1", "cost");
        std::fs::create_dir_all(&qdir).unwrap();
        std::fs::write(
            qdir.join("queue.json"),
            r#"{"id":2,"name":"Cost Invoices","url":"rdc://queues/cost","schema":"rdc://schemas/cost"}"#,
        )
        .unwrap();
        std::fs::write(
            qdir.join("schema.json"),
            r#"{"id":3,"name":"S","url":"rdc://schemas/cost","queues":["rdc://queues/cost"]}"#,
        )
        .unwrap();
        std::fs::write(
            paths.workspace_dir("ws1").join("workspace.json"),
            r#"{"id":1,"name":"Ws1","url":"rdc://workspaces/ws1","queues":["rdc://queues/cost"]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();
        std::fs::write(
            paths.hooks_dir().join("h1.json"),
            r#"{"id":9,"name":"h1","url":"rdc://hooks/h1","queues":["rdc://queues/cost"]}"#,
        )
        .unwrap();

        let mut lockfile = Lockfile::default();
        for (kind, slug, id) in [
            ("workspaces", "ws1", 1u64),
            ("queues", "cost", 2),
            ("schemas", "cost", 3),
            ("hooks", "h1", 9),
        ] {
            lockfile.upsert(
                kind,
                slug,
                ObjectEntry { id, modified_at: None, content_hash: None, secrets_hash: None },
            );
        }

        let pending = detect(&paths, &lockfile);
        assert_eq!(
            pending,
            vec![PendingRename::Queue {
                ws: "ws1".into(),
                old: "cost".into(),
                new: "cost-invoices".into(),
            }]
        );
        apply(&paths, &mut lockfile, pending, false).unwrap();

        let qdir_new = paths.queue_dir("ws1", "cost-invoices");
        let q = std::fs::read_to_string(qdir_new.join("queue.json")).unwrap();
        assert!(q.contains(r#""rdc://queues/cost-invoices""#) && q.contains(r#""rdc://schemas/cost-invoices""#), "queue.json refs: {q}");
        let s = std::fs::read_to_string(qdir_new.join("schema.json")).unwrap();
        assert!(s.contains(r#""rdc://schemas/cost-invoices""#) && s.contains(r#""rdc://queues/cost-invoices""#), "schema.json refs: {s}");
        let ws = std::fs::read_to_string(paths.workspace_dir("ws1").join("workspace.json")).unwrap();
        assert!(ws.contains(r#""rdc://queues/cost-invoices""#), "workspace refs: {ws}");
        let h = std::fs::read_to_string(paths.hooks_dir().join("h1.json")).unwrap();
        assert!(h.contains(r#""rdc://queues/cost-invoices""#), "hook refs: {h}");
        // No stale old-slug refs anywhere.
        for body in [&q, &s, &ws, &h] {
            assert!(!body.contains(r#"/cost""#), "stale old slug remains: {body}");
        }
    }

    /// Portable-refs gap #3: a rename must move the base-cache sidecar to the
    /// new slug so the next sync keeps a 3-way merge base (otherwise a pure
    /// local rename degrades to a spurious "both diverged" conflict).
    #[test]
    fn apply_label_rename_moves_base_cache_sidecar() {
        use crate::state::ObjectEntry;
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.labels_dir()).unwrap();
        std::fs::write(
            paths.labels_dir().join("hold.json"),
            r#"{"id":1,"name":"Audit Hold","url":"rdc://labels/hold"}"#,
        )
        .unwrap();
        let base_old = paths.base_cache_root().join("labels").join("hold.json");
        std::fs::create_dir_all(base_old.parent().unwrap()).unwrap();
        std::fs::write(&base_old, r#"{"id":1,"name":"Audit Hold","url":"rdc://labels/hold"}"#).unwrap();

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "labels",
            "hold",
            ObjectEntry { id: 1, modified_at: None, content_hash: None, secrets_hash: None },
        );

        let pending = detect(&paths, &lockfile);
        assert_eq!(
            pending,
            vec![PendingRename::Label { old: "hold".into(), new: "audit-hold".into() }]
        );
        apply(&paths, &mut lockfile, pending, false).unwrap();

        let base_new = paths.base_cache_root().join("labels").join("audit-hold.json");
        assert!(base_new.exists(), "base-cache sidecar must move to the new slug");
        assert!(!base_old.exists(), "old-slug base-cache sidecar must be gone");
    }

    /// Portable-refs gap #3b: a referencing object's *base-cache* copy must also
    /// have its `rdc://<kind>/<old>` ref rewritten. The base mirrors the
    /// portabilized remote, whose slug follows the lockfile; if the base keeps
    /// the old ref while the env file gets the new one, the next sync sees
    /// local≠base even though local==remote and raises a spurious conflict.
    #[test]
    fn apply_rename_rewrites_refs_in_base_cache_of_referencing_objects() {
        use crate::state::ObjectEntry;
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.hooks_dir()).unwrap();
        // Env file references the hook being renamed.
        std::fs::write(
            paths.hooks_dir().join("val.json"),
            r#"{"id":1,"name":"Validator V2","url":"rdc://hooks/val","queues":[]}"#,
        )
        .unwrap();
        let qdir = paths.queue_dir("ws1", "q1");
        std::fs::create_dir_all(&qdir).unwrap();
        std::fs::write(
            qdir.join("queue.json"),
            r#"{"id":2,"name":"Q1","url":"rdc://queues/q1","hooks":["rdc://hooks/val"]}"#,
        )
        .unwrap();
        // Base-cache copy of the referencing queue still holds the OLD ref.
        let base_q = paths
            .base_cache_root()
            .join("workspaces/ws1/queues/q1/queue.json");
        std::fs::create_dir_all(base_q.parent().unwrap()).unwrap();
        std::fs::write(
            &base_q,
            r#"{"id":2,"name":"Q1","url":"rdc://queues/q1","hooks":["rdc://hooks/val"]}"#,
        )
        .unwrap();

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            "val",
            ObjectEntry { id: 1, modified_at: None, content_hash: Some("h".into()), secrets_hash: None },
        );
        lockfile.upsert(
            "queues",
            "q1",
            ObjectEntry { id: 2, modified_at: None, content_hash: None, secrets_hash: None },
        );

        let pending = detect(&paths, &lockfile);
        apply(&paths, &mut lockfile, pending, false).unwrap();

        let base = std::fs::read_to_string(&base_q).unwrap();
        assert!(
            base.contains(r#""rdc://hooks/validator-v2""#),
            "base-cache ref must be rewritten too, got: {base}"
        );
        assert!(
            !base.contains(r#""rdc://hooks/val""#),
            "old base-cache ref must be gone, got: {base}"
        );
    }

    /// Portable-refs gap #5: a queue rename must rewrite the COMPOUND own-`url`
    /// of every email template under it (`rdc://email_templates/<ws>/<q>/<t>`),
    /// whose middle segment embeds the queue slug. A whole-token sweep misses
    /// these (the queue slug is mid-string, not the whole token), leaving the
    /// template's own url stale and forcing a spurious PATCH on the next sync.
    #[test]
    fn apply_queue_rename_rewrites_email_template_compound_own_urls() {
        use crate::state::ObjectEntry;
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let qdir = paths.queue_dir("ws1", "cost");
        std::fs::create_dir_all(&qdir).unwrap();
        std::fs::write(
            qdir.join("queue.json"),
            r#"{"id":2,"name":"Cost Invoices","url":"rdc://queues/cost","schema":"rdc://schemas/cost"}"#,
        )
        .unwrap();
        let et_dir = paths.queue_email_templates_dir("ws1", "cost");
        std::fs::create_dir_all(&et_dir).unwrap();
        std::fs::write(
            et_dir.join("welcome.json"),
            r#"{"id":7,"name":"Welcome","url":"rdc://email_templates/ws1/cost/welcome","queue":"rdc://queues/cost"}"#,
        )
        .unwrap();
        // detect()'s queue locator needs the parent workspace in the lockfile.
        std::fs::create_dir_all(paths.workspace_dir("ws1")).unwrap();

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "workspaces",
            "ws1",
            ObjectEntry { id: 9, modified_at: None, content_hash: None, secrets_hash: None },
        );
        for (kind, slug, id) in [("queues", "cost", 2u64), ("schemas", "cost", 3)] {
            lockfile.upsert(
                kind,
                slug,
                ObjectEntry { id, modified_at: None, content_hash: None, secrets_hash: None },
            );
        }
        lockfile.upsert(
            "email_templates",
            "ws1/cost/welcome",
            ObjectEntry { id: 7, modified_at: None, content_hash: None, secrets_hash: None },
        );

        let pending = detect(&paths, &lockfile);
        assert_eq!(
            pending,
            vec![PendingRename::Queue { ws: "ws1".into(), old: "cost".into(), new: "cost-invoices".into() }]
        );
        apply(&paths, &mut lockfile, pending, false).unwrap();

        let et = std::fs::read_to_string(
            paths.queue_email_templates_dir("ws1", "cost-invoices").join("welcome.json"),
        )
        .unwrap();
        assert!(
            et.contains(r#""rdc://email_templates/ws1/cost-invoices/welcome""#),
            "email template compound own-url must follow the queue rename, got: {et}"
        );
        assert!(
            !et.contains(r#""rdc://email_templates/ws1/cost/welcome""#),
            "stale compound own-url must be gone, got: {et}"
        );
        // The plain queue ref is also rewritten (whole-token path).
        assert!(et.contains(r#""rdc://queues/cost-invoices""#), "queue ref: {et}");
    }

    /// Portable-refs gap #6a: an engine rename must cascade to its fields'
    /// compound lockfile keys (`<engine>/<field>`) AND rewrite each field's
    /// compound own-`url` (`rdc://engine_fields/<engine>/<field>`). Symmetric
    /// with the queue→email-template cascade.
    #[test]
    fn apply_engine_rename_cascades_field_keys_and_own_urls() {
        use crate::state::ObjectEntry;
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.engine_fields_dir("old-eng")).unwrap();
        std::fs::write(
            paths.engine_dir("old-eng").join("engine.json"),
            r#"{"id":1,"name":"New Engine","url":"rdc://engines/old-eng"}"#,
        )
        .unwrap();
        std::fs::write(
            paths.engine_fields_dir("old-eng").join("amount.json"),
            r#"{"id":2,"name":"amount","url":"rdc://engine_fields/old-eng/amount","engine":"rdc://engines/old-eng"}"#,
        )
        .unwrap();

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "engines",
            "old-eng",
            ObjectEntry { id: 1, modified_at: None, content_hash: None, secrets_hash: None },
        );
        lockfile.upsert(
            "engine_fields",
            "old-eng/amount",
            ObjectEntry { id: 2, modified_at: None, content_hash: None, secrets_hash: None },
        );

        let pending = detect(&paths, &lockfile);
        assert_eq!(
            pending,
            vec![PendingRename::Engine { old: "old-eng".into(), new: "new-engine".into() }]
        );
        apply(&paths, &mut lockfile, pending, false).unwrap();

        // Compound key cascaded.
        let ef = lockfile.objects.get("engine_fields").unwrap();
        assert!(ef.contains_key("new-engine/amount"), "field key cascaded: {:?}", ef.keys().collect::<Vec<_>>());
        assert!(!ef.contains_key("old-eng/amount"), "old field key gone");
        // Field's compound own-url rewritten + its engine ref (whole-token).
        let field = std::fs::read_to_string(
            paths.engine_fields_dir("new-engine").join("amount.json"),
        )
        .unwrap();
        assert!(
            field.contains(r#""rdc://engine_fields/new-engine/amount""#),
            "field compound own-url must follow engine rename, got: {field}"
        );
        assert!(field.contains(r#""rdc://engines/new-engine""#), "engine ref rewritten: {field}");
        assert!(!field.contains("old-eng"), "no stale engine slug: {field}");
    }

    /// Portable-refs gap #6b: same cascade for workflow → workflow steps
    /// (`rdc://workflow_steps/<workflow>/<step>`).
    #[test]
    fn apply_workflow_rename_cascades_step_keys_and_own_urls() {
        use crate::state::ObjectEntry;
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.workflow_dir("old-wf").join("steps")).unwrap();
        std::fs::write(
            paths.workflow_dir("old-wf").join("workflow.json"),
            r#"{"id":1,"name":"New Flow","url":"rdc://workflows/old-wf"}"#,
        )
        .unwrap();
        std::fs::write(
            paths.workflow_dir("old-wf").join("steps").join("review.json"),
            r#"{"id":2,"name":"review","url":"rdc://workflow_steps/old-wf/review","workflow":"rdc://workflows/old-wf"}"#,
        )
        .unwrap();
        // A queue references the workflow (workflows ARE ref targets).
        let qdir = paths.queue_dir("ws1", "q1");
        std::fs::create_dir_all(&qdir).unwrap();
        std::fs::write(
            qdir.join("queue.json"),
            r#"{"id":5,"name":"Q1","url":"rdc://queues/q1","workflows":[{"url":"rdc://workflows/old-wf","priority":1}]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(paths.workspace_dir("ws1")).unwrap();

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "workspaces",
            "ws1",
            ObjectEntry { id: 9, modified_at: None, content_hash: None, secrets_hash: None },
        );
        lockfile.upsert(
            "queues",
            "q1",
            ObjectEntry { id: 5, modified_at: None, content_hash: None, secrets_hash: None },
        );
        lockfile.upsert(
            "workflows",
            "old-wf",
            ObjectEntry { id: 1, modified_at: None, content_hash: None, secrets_hash: None },
        );
        lockfile.upsert(
            "workflow_steps",
            "old-wf/review",
            ObjectEntry { id: 2, modified_at: None, content_hash: None, secrets_hash: None },
        );

        let pending = detect(&paths, &lockfile);
        assert_eq!(
            pending,
            vec![PendingRename::Workflow { old: "old-wf".into(), new: "new-flow".into() }]
        );
        apply(&paths, &mut lockfile, pending, false).unwrap();

        let ws = lockfile.objects.get("workflow_steps").unwrap();
        assert!(ws.contains_key("new-flow/review"), "step key cascaded: {:?}", ws.keys().collect::<Vec<_>>());
        assert!(!ws.contains_key("old-wf/review"), "old step key gone");
        // Workflow's own url (whole-token) rewritten.
        let wf = std::fs::read_to_string(paths.workflow_dir("new-flow").join("workflow.json")).unwrap();
        assert!(wf.contains(r#""rdc://workflows/new-flow""#), "workflow own url: {wf}");
        // Step: compound own-url AND its `workflow` ref rewritten.
        let step = std::fs::read_to_string(
            paths.workflow_dir("new-flow").join("steps").join("review.json"),
        )
        .unwrap();
        assert!(
            step.contains(r#""rdc://workflow_steps/new-flow/review""#),
            "step compound own-url must follow workflow rename, got: {step}"
        );
        assert!(step.contains(r#""rdc://workflows/new-flow""#), "step workflow ref: {step}");
        assert!(!step.contains("old-wf"), "no stale workflow slug in step: {step}");
        // Referencing queue's workflows[] ref rewritten.
        let q = std::fs::read_to_string(qdir.join("queue.json")).unwrap();
        assert!(q.contains(r#""rdc://workflows/new-flow""#), "queue workflow ref: {q}");
        assert!(!q.contains("old-wf"), "no stale workflow slug in queue: {q}");
    }
}
