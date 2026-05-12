//! Within-env slug realignment.
//!
//! Invoked via `rdc map <env>` (single-arg form of `rdc map`). Walks the
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
    Workspace { old: String, new: String },
    Queue { ws: String, old: String, new: String },
    EmailTemplate { ws: String, q: String, old: String, new: String },
    Hook { old: String, new: String },
    Rule { old: String, new: String },
    Label { old: String, new: String },
    Engine { old: String, new: String },
    EngineField { old: String, new: String },
    Workflow { old: String, new: String },
    WorkflowStep { old: String, new: String },
}

impl PendingRename {
    /// One-line summary for the interactive prompt and `--check` listing.
    pub fn describe(&self) -> String {
        match self {
            PendingRename::Workspace { old, new } => format!("workspaces/{old} → {new}"),
            PendingRename::Queue { ws, old, new } => format!("queues/{ws}/{old} → {new}"),
            PendingRename::EmailTemplate { ws, q, old, new } => {
                format!("email_templates/{ws}/{q}/{old} → {new}")
            }
            PendingRename::Hook { old, new } => format!("hooks/{old} → {new}"),
            PendingRename::Rule { old, new } => format!("rules/{old} → {new}"),
            PendingRename::Label { old, new } => format!("labels/{old} → {new}"),
            PendingRename::Engine { old, new } => format!("engines/{old} → {new}"),
            PendingRename::EngineField { old, new } => format!("engine_fields/{old} → {new}"),
            PendingRename::Workflow { old, new } => format!("workflows/{old} → {new}"),
            PendingRename::WorkflowStep { old, new } => format!("workflow_steps/{old} → {new}"),
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
            if let Some((ws, q_path)) = locate_queue_dir(paths, lockfile, slug) {
                if let Some(name) = read_name(&q_path.join("queue.json")) {
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
            let Some((ws, q, t)) = split_compound(compound) else { continue };
            let tpl_path = paths
                .queue_email_templates_dir(&ws, &q)
                .join(format!("{t}.json"));
            let Some(name) = read_name(&tpl_path) else { continue };
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
    let Some(by_slug) = lockfile.objects.get(kind) else { return };
    for slug in by_slug.keys() {
        let file = dir.join(format!("{slug}.json"));
        let Some(name) = read_name(&file) else { continue };
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
    v.get("name").and_then(|n| n.as_str()).map(|s| s.to_string())
}

/// Detect stale engine slugs: read `engines/<slug>/engine.json` and
/// propose a rename if the JSON's `name` slugifies to something
/// different than `<slug>`. Engines own a directory on disk, so a
/// rename moves the whole subtree (engine.json + fields/).
fn detect_engines(paths: &Paths, lockfile: &Lockfile, out: &mut Vec<PendingRename>) {
    let Some(by_slug) = lockfile.objects.get("engines") else { return };
    for slug in by_slug.keys() {
        let file = paths.engine_dir(slug).join("engine.json");
        let Some(name) = read_name(&file) else { continue };
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
/// engine; we find that engine by walking and matching.
fn detect_engine_fields(paths: &Paths, lockfile: &Lockfile, out: &mut Vec<PendingRename>) {
    let Some(by_slug) = lockfile.objects.get("engine_fields") else { return };
    for slug in by_slug.keys() {
        // Walk every engine until we find the field.
        let Some(file) = locate_engine_field_file(paths, slug) else { continue };
        let Some(name) = read_name(&file) else { continue };
        let proposed = slugify(&name);
        if proposed != *slug && !by_slug.contains_key(&proposed) {
            out.push(PendingRename::EngineField {
                old: slug.clone(),
                new: proposed,
            });
        }
    }
}

/// Detect stale workflow slugs. Same pattern as engines.
fn detect_workflows(paths: &Paths, lockfile: &Lockfile, out: &mut Vec<PendingRename>) {
    let Some(by_slug) = lockfile.objects.get("workflows") else { return };
    for slug in by_slug.keys() {
        let file = paths.workflow_dir(slug).join("workflow.json");
        let Some(name) = read_name(&file) else { continue };
        let proposed = slugify(&name);
        if proposed != *slug && !by_slug.contains_key(&proposed) {
            out.push(PendingRename::Workflow {
                old: slug.clone(),
                new: proposed,
            });
        }
    }
}

/// Detect stale workflow-step slugs. Same pattern as engine_fields.
fn detect_workflow_steps(paths: &Paths, lockfile: &Lockfile, out: &mut Vec<PendingRename>) {
    let Some(by_slug) = lockfile.objects.get("workflow_steps") else { return };
    for slug in by_slug.keys() {
        let Some(file) = locate_workflow_step_file(paths, slug) else { continue };
        let Some(name) = read_name(&file) else { continue };
        let proposed = slugify(&name);
        if proposed != *slug && !by_slug.contains_key(&proposed) {
            out.push(PendingRename::WorkflowStep {
                old: slug.clone(),
                new: proposed,
            });
        }
    }
}

/// Walk `engines/*/fields/<slug>.json` and return the first match.
fn locate_engine_field_file(paths: &Paths, slug: &str) -> Option<std::path::PathBuf> {
    let engines_dir = paths.engines_dir();
    let entries = std::fs::read_dir(&engines_dir).ok()?;
    for e_entry in entries.flatten() {
        if !e_entry.file_type().ok()?.is_dir() {
            continue;
        }
        let e_slug = e_entry.file_name().to_string_lossy().to_string();
        let p = paths.engine_fields_dir(&e_slug).join(format!("{slug}.json"));
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Walk `workflows/*/steps/<slug>.json` and return the first match.
fn locate_workflow_step_file(paths: &Paths, slug: &str) -> Option<std::path::PathBuf> {
    let workflows_dir = paths.workflows_dir();
    let entries = std::fs::read_dir(&workflows_dir).ok()?;
    for w_entry in entries.flatten() {
        if !w_entry.file_type().ok()?.is_dir() {
            continue;
        }
        let w_slug = w_entry.file_name().to_string_lossy().to_string();
        let p = paths.workflow_steps_dir(&w_slug).join(format!("{slug}.json"));
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
            if lockfile.objects.get("workspaces").is_some_and(|m| m.contains_key(&ws_slug)) {
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
    Some((parts[0].to_string(), parts[1].to_string(), parts[2].to_string()))
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

    Ok(stats)
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
                if let PendingRename::EmailTemplate { q, .. } = p {
                    if q == old {
                        *q = new.clone();
                    }
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

fn apply_one(
    paths: &Paths,
    lockfile: &mut Lockfile,
    p: &PendingRename,
) -> Result<Vec<String>> {
    let mut orphans: Vec<String> = Vec::new();
    match p {
        PendingRename::Hook { old, new } => {
            move_file(&paths.hooks_dir().join(format!("{old}.json")), &paths.hooks_dir().join(format!("{new}.json")))?;
            move_optional(
                &paths.hooks_dir().join(format!("{old}.py")),
                &paths.hooks_dir().join(format!("{new}.py")),
            )?;
            rename_lockfile_key(lockfile, "hooks", old, new);
            collect_orphans(paths, "hooks", old, &mut orphans);
        }
        PendingRename::Rule { old, new } => {
            // Rules are a combined-form kind (json + optional
            // trigger_condition .py). Move both files when present.
            move_file(&paths.rules_dir().join(format!("{old}.json")), &paths.rules_dir().join(format!("{new}.json")))?;
            move_optional(
                &paths.rules_dir().join(format!("{old}.py")),
                &paths.rules_dir().join(format!("{new}.py")),
            )?;
            rename_lockfile_key(lockfile, "rules", old, new);
            collect_orphans(paths, "rules", old, &mut orphans);
        }
        PendingRename::Label { old, new } => {
            move_file(&paths.labels_dir().join(format!("{old}.json")), &paths.labels_dir().join(format!("{new}.json")))?;
            rename_lockfile_key(lockfile, "labels", old, new);
            collect_orphans(paths, "labels", old, &mut orphans);
        }
        PendingRename::Engine { old, new } => {
            // Engines own a dir (engine.json + fields/); a rename moves
            // the whole subtree. Engine-field slugs are unchanged.
            move_dir(&paths.engine_dir(old), &paths.engine_dir(new))?;
            rename_lockfile_key(lockfile, "engines", old, new);
            collect_orphans(paths, "engines", old, &mut orphans);
        }
        PendingRename::EngineField { old, new } => {
            // Find the field under whatever engine currently owns it,
            // then rename in place inside that engine's fields/ dir.
            if let Some(old_path) = locate_engine_field_file(paths, old) {
                if let Some(parent) = old_path.parent() {
                    move_file(&old_path, &parent.join(format!("{new}.json")))?;
                }
            }
            rename_lockfile_key(lockfile, "engine_fields", old, new);
            collect_orphans(paths, "engine_fields", old, &mut orphans);
        }
        PendingRename::Workflow { old, new } => {
            // Workflows own a dir (workflow.json + steps/); same shape
            // as engines.
            move_dir(&paths.workflow_dir(old), &paths.workflow_dir(new))?;
            rename_lockfile_key(lockfile, "workflows", old, new);
            collect_orphans(paths, "workflows", old, &mut orphans);
        }
        PendingRename::WorkflowStep { old, new } => {
            if let Some(old_path) = locate_workflow_step_file(paths, old) {
                if let Some(parent) = old_path.parent() {
                    move_file(&old_path, &parent.join(format!("{new}.json")))?;
                }
            }
            rename_lockfile_key(lockfile, "workflow_steps", old, new);
        }
        PendingRename::EmailTemplate { ws, q, old, new } => {
            let dir = paths.queue_email_templates_dir(ws, q);
            move_file(&dir.join(format!("{old}.json")), &dir.join(format!("{new}.json")))?;
            let old_compound = format!("{ws}/{q}/{old}");
            let new_compound = format!("{ws}/{q}/{new}");
            rename_lockfile_key(lockfile, "email_templates", &old_compound, &new_compound);
            collect_orphans(paths, "email_templates", &old_compound, &mut orphans);
        }
        PendingRename::Workspace { old, new } => {
            move_dir(&paths.workspace_dir(old), &paths.workspace_dir(new))?;
            rename_lockfile_key(lockfile, "workspaces", old, new);
            rewrite_email_template_compound_prefix(lockfile, 0, old, new);
            collect_orphans(paths, "workspaces", old, &mut orphans);
        }
        PendingRename::Queue { ws, old, new } => {
            move_dir(&paths.queue_dir(ws, old), &paths.queue_dir(ws, new))?;
            rename_lockfile_key(lockfile, "queues", old, new);
            rename_lockfile_key(lockfile, "schemas", old, new);
            rename_lockfile_key(lockfile, "inboxes", old, new);
            rewrite_email_template_compound_prefix(lockfile, 1, old, new);
            collect_orphans(paths, "queues", old, &mut orphans);
        }
    }
    Ok(orphans)
}

fn move_file(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    if to.exists() {
        anyhow::bail!("destination {} already exists", to.display());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::rename(from, to)
        .with_context(|| format!("moving {} → {}", from.display(), to.display()))
}

fn move_optional(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    if !from.exists() {
        return Ok(());
    }
    move_file(from, to)
}

fn move_dir(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    if to.exists() {
        anyhow::bail!("destination {} already exists", to.display());
    }
    std::fs::rename(from, to)
        .with_context(|| format!("moving {} → {}", from.display(), to.display()))
}

/// Move a slug-keyed entry within a lockfile kind.
fn rename_lockfile_key(lockfile: &mut Lockfile, kind: &str, old: &str, new: &str) {
    let Some(by_slug) = lockfile.objects.get_mut(kind) else { return };
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
    let Some(by_key) = lockfile.objects.get_mut("email_templates") else { return };
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
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        if raw.contains(&needle) || raw.contains(&dotted) {
            let msg = format!(
                "  {} references {kind}/{old} — update manually",
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
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect()
}

/// Entry point used by `cli::run` when `rdc map <env>` is invoked.
pub async fn run_within_env(env: &str, check: bool, yes: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let paths = Paths::for_env(&cwd, env);
    let cfg = crate::config::ProjectConfig::load(&paths.project_config())
        .with_context(|| format!("loading project config from {}", paths.project_config().display()))?;
    if !cfg.envs.contains_key(env) {
        anyhow::bail!("env '{env}' is not defined in rdc.toml");
    }
    let mut lockfile = Lockfile::load(&paths.lockfile())?;

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
        let h = PendingRename::Hook { old: "a".into(), new: "b".into() };
        let q = PendingRename::Queue { ws: "ws".into(), old: "a".into(), new: "b".into() };
        let w = PendingRename::Workspace { old: "a".into(), new: "b".into() };
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
            PendingRename::Queue { ws: "old-ws".into(), old: "q1".into(), new: "q1-new".into() },
            PendingRename::EmailTemplate {
                ws: "old-ws".into(), q: "q1".into(), old: "t1".into(), new: "t1-new".into(),
            },
            PendingRename::Hook { old: "h1".into(), new: "h1-new".into() },
        ];
        let applied = PendingRename::Workspace { old: "old-ws".into(), new: "new-ws".into() };
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
            ws: "ws".into(), q: "q-old".into(), old: "t1".into(), new: "t1-new".into(),
        }];
        let applied = PendingRename::Queue { ws: "ws".into(), old: "q-old".into(), new: "q-new".into() };
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
        lf.upsert("email_templates", "ws-old/q1/t1", ObjectEntry { id: 1, url: None, modified_at: None, content_hash: None });
        lf.upsert("email_templates", "ws-old/q2/t2", ObjectEntry { id: 2, url: None, modified_at: None, content_hash: None });
        lf.upsert("email_templates", "ws-other/q3/t3", ObjectEntry { id: 3, url: None, modified_at: None, content_hash: None });
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
        lf.upsert("email_templates", "ws/q-old/t1", ObjectEntry { id: 1, url: None, modified_at: None, content_hash: None });
        lf.upsert("email_templates", "ws/q-other/t2", ObjectEntry { id: 2, url: None, modified_at: None, content_hash: None });
        rewrite_email_template_compound_prefix(&mut lf, 1, "q-old", "q-new");
        let keys: Vec<&String> = lf.objects.get("email_templates").unwrap().keys().collect();
        assert!(keys.contains(&&"ws/q-new/t1".to_string()));
        assert!(keys.contains(&&"ws/q-other/t2".to_string()));
    }

    #[test]
    fn rename_lockfile_key_moves_entry() {
        use crate::state::ObjectEntry;
        let mut lf = Lockfile::default();
        let entry = ObjectEntry { id: 42, url: None, modified_at: None, content_hash: Some("abc".into()) };
        lf.upsert("hooks", "old", entry.clone());
        rename_lockfile_key(&mut lf, "hooks", "old", "new");
        assert!(!lf.objects.get("hooks").unwrap().contains_key("old"));
        let got = lf.objects.get("hooks").unwrap().get("new").unwrap();
        assert_eq!(got.id, 42);
        assert_eq!(got.content_hash.as_deref(), Some("abc"));
    }

    #[test]
    fn describe_formats_each_kind() {
        let h = PendingRename::Hook { old: "a".into(), new: "b".into() };
        assert_eq!(h.describe(), "hooks/a → b");
        let q = PendingRename::Queue { ws: "ws".into(), old: "a".into(), new: "b".into() };
        assert_eq!(q.describe(), "queues/ws/a → b");
        let t = PendingRename::EmailTemplate { ws: "ws".into(), q: "q".into(), old: "t".into(), new: "t2".into() };
        assert_eq!(t.describe(), "email_templates/ws/q/t → t2");
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
        ).unwrap();
        std::fs::write(
            paths.hooks_dir().join("validator-invoices.py"),
            b"def run(p): return p",
        ).unwrap();

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "hooks",
            "validator-invoices",
            ObjectEntry { id: 1, url: None, modified_at: None, content_hash: Some("h".into()) },
        );

        let pending = detect(&paths, &lockfile);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0], PendingRename::Hook {
            old: "validator-invoices".into(),
            new: "validator-invoices-v2".into(),
        });

        let stats = apply(&paths, &mut lockfile, pending, false).unwrap();
        assert_eq!(stats.applied, 1);
        assert!(paths.hooks_dir().join("validator-invoices-v2.json").exists());
        assert!(paths.hooks_dir().join("validator-invoices-v2.py").exists());
        assert!(!paths.hooks_dir().join("validator-invoices.json").exists());
        assert!(!paths.hooks_dir().join("validator-invoices.py").exists());
        assert!(lockfile.objects.get("hooks").unwrap().contains_key("validator-invoices-v2"));
        assert!(!lockfile.objects.get("hooks").unwrap().contains_key("validator-invoices"));
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
        ).unwrap();

        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "workspaces", "old-ws",
            ObjectEntry { id: 1, url: None, modified_at: None, content_hash: None },
        );
        lockfile.upsert(
            "email_templates", "old-ws/q1/t1",
            ObjectEntry { id: 10, url: None, modified_at: None, content_hash: None },
        );
        lockfile.upsert(
            "email_templates", "old-ws/q1/t2",
            ObjectEntry { id: 11, url: None, modified_at: None, content_hash: None },
        );
        lockfile.upsert(
            "email_templates", "other-ws/q9/t9",
            ObjectEntry { id: 99, url: None, modified_at: None, content_hash: None },
        );

        let pending = detect(&paths, &lockfile);
        assert_eq!(pending.len(), 1);

        let stats = apply(&paths, &mut lockfile, pending, false).unwrap();
        assert_eq!(stats.applied, 1);
        assert!(paths.workspace_dir("new-workspace").exists());
        assert!(!paths.workspace_dir("old-ws").exists());
        let et_keys: Vec<&String> = lockfile.objects.get("email_templates").unwrap().keys().collect();
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
        ).unwrap();
        std::fs::write(queue_dir.join("schema.json"), b"{}").unwrap();
        std::fs::write(queue_dir.join("inbox.json"), b"{}").unwrap();
        // Need workspace dir for lockfile consistency check in detect.
        std::fs::create_dir_all(paths.workspace_dir("ws1")).unwrap();

        let mut lockfile = Lockfile::default();
        lockfile.upsert("workspaces", "ws1",
            ObjectEntry { id: 9, url: None, modified_at: None, content_hash: None });
        lockfile.upsert("queues", "cost-invoices",
            ObjectEntry { id: 1, url: None, modified_at: None, content_hash: None });
        lockfile.upsert("schemas", "cost-invoices",
            ObjectEntry { id: 2, url: None, modified_at: None, content_hash: None });
        lockfile.upsert("inboxes", "cost-invoices",
            ObjectEntry { id: 3, url: None, modified_at: None, content_hash: None });
        lockfile.upsert("email_templates", "ws1/cost-invoices/welcome",
            ObjectEntry { id: 4, url: None, modified_at: None, content_hash: None });
        lockfile.upsert("email_templates", "ws1/cost-invoices/rejected",
            ObjectEntry { id: 5, url: None, modified_at: None, content_hash: None });

        let pending = detect(&paths, &lockfile);
        let queue_pending: Vec<_> = pending.iter()
            .filter(|p| matches!(p, PendingRename::Queue { .. }))
            .collect();
        assert_eq!(queue_pending.len(), 1);

        let stats = apply(&paths, &mut lockfile, pending, false).unwrap();
        assert!(stats.applied >= 1);
        assert!(paths.queue_dir("ws1", "ap-invoices").exists());
        assert!(!paths.queue_dir("ws1", "cost-invoices").exists());
        // All three queue-keyed lockfile entries moved.
        assert!(lockfile.objects.get("queues").unwrap().contains_key("ap-invoices"));
        assert!(lockfile.objects.get("schemas").unwrap().contains_key("ap-invoices"));
        assert!(lockfile.objects.get("inboxes").unwrap().contains_key("ap-invoices"));
        // Old keys gone.
        assert!(!lockfile.objects.get("queues").unwrap().contains_key("cost-invoices"));
        assert!(!lockfile.objects.get("schemas").unwrap().contains_key("cost-invoices"));
        assert!(!lockfile.objects.get("inboxes").unwrap().contains_key("cost-invoices"));
        // Email_template compound middle segment rewritten.
        let et_keys: Vec<&String> = lockfile.objects.get("email_templates").unwrap().keys().collect();
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
        ).unwrap();
        // hook-b already exists in the lockfile (some other hook).
        std::fs::write(
            paths.hooks_dir().join("hook-b.json"),
            r#"{"id":2,"name":"Hook B","queues":[]}"#,
        ).unwrap();
        let mut lockfile = Lockfile::default();
        lockfile.upsert("hooks", "hook-a",
            ObjectEntry { id: 1, url: None, modified_at: None, content_hash: None });
        lockfile.upsert("hooks", "hook-b",
            ObjectEntry { id: 2, url: None, modified_at: None, content_hash: None });
        let pending = detect(&paths, &lockfile);
        assert!(pending.is_empty(), "expected no rename (would collide), got {pending:?}");
    }
}
