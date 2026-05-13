//! Slug-to-slug auto-matching across two local snapshots. Used by
//! `rdc deploy` to populate the `Mapping` before its plan + execute
//! phases, so existing same-slug objects in both envs get linked
//! without the user having to run a separate command.
//!
//! There used to be a top-level `rdc map <src> <tgt>` command that
//! exposed this same logic and wrote the result to disk; with `rdc
//! deploy` now owning the full cross-env workflow, that surface is
//! gone and this module is internal.

use crate::mapping::Mapping;
use crate::paths::Paths;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// Augment `mapping` with same-slug entries for every kind where both
/// `src` and `tgt` already have a local file. Existing entries (including
/// hand-curated cross-env renames in the mapping file) are preserved —
/// auto-match never overwrites.
///
/// Returns the total number of newly-added entries across all kinds.
pub fn auto_match(mapping: &mut Mapping, src_paths: &Paths, tgt_paths: &Paths) -> Result<usize> {
    let mut added = 0;
    added += match_workspaces(&mut mapping.workspaces, src_paths, tgt_paths)?;
    added += match_kind(&mut mapping.hooks, &src_paths.hooks_dir(), &tgt_paths.hooks_dir())?;
    added += match_kind(&mut mapping.rules, &src_paths.rules_dir(), &tgt_paths.rules_dir())?;
    added += match_kind(&mut mapping.labels, &src_paths.labels_dir(), &tgt_paths.labels_dir())?;
    added += match_queues(&mut mapping.queues, src_paths, tgt_paths)?;
    added += match_schemas(&mut mapping.schemas, src_paths, tgt_paths)?;
    added += match_inboxes(&mut mapping.inboxes, src_paths, tgt_paths)?;
    added += match_email_templates(&mut mapping.email_templates, src_paths, tgt_paths)?;
    added += match_engines(&mut mapping.engines, src_paths, tgt_paths)?;
    added += match_engine_fields(&mut mapping.engine_fields, src_paths, tgt_paths)?;
    Ok(added)
}

fn match_kind(
    existing: &mut BTreeMap<String, String>,
    src_dir: &Path,
    tgt_dir: &Path,
) -> Result<usize> {
    let src_slugs = list_flat_slugs(src_dir)?;
    let tgt_slugs: std::collections::HashSet<_> = list_flat_slugs(tgt_dir)?.into_iter().collect();
    let mut added = 0;
    for src_slug in &src_slugs {
        if existing.contains_key(src_slug) {
            continue;
        }
        if tgt_slugs.contains(src_slug) {
            existing.insert(src_slug.clone(), src_slug.clone());
            added += 1;
        }
    }
    Ok(added)
}

fn list_flat_slugs(dir: &Path) -> Result<Vec<String>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
    {
        let entry = entry
            .with_context(|| format!("listing {}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(slug) = name.strip_suffix(".json") {
            if !slug.ends_with(".remote") {
                out.push(slug.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// List engine slugs from `engines/<slug>/engine.json` layout.
fn list_engine_slugs(paths: &Paths) -> Result<Vec<String>> {
    let engines_dir = paths.engines_dir();
    if !engines_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&engines_dir)
        .with_context(|| format!("reading {}", engines_dir.display()))?
    {
        let entry = entry.with_context(|| format!("listing {}", engines_dir.display()))?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let slug = entry.file_name().to_string_lossy().to_string();
        // Only count engines that have their JSON on disk.
        if entry.path().join("engine.json").exists() {
            out.push(slug);
        }
    }
    out.sort();
    Ok(out)
}

/// List engine-field slugs across every engine. Slugs are globally
/// unique so the parent-engine slug doesn't appear in the result.
fn list_engine_field_slugs(paths: &Paths) -> Result<Vec<String>> {
    let engines_dir = paths.engines_dir();
    if !engines_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for e_entry in std::fs::read_dir(&engines_dir)? {
        let e_entry = e_entry?;
        if !e_entry.file_type()?.is_dir() {
            continue;
        }
        let e_slug = e_entry.file_name().to_string_lossy().to_string();
        let fields_dir = paths.engine_fields_dir(&e_slug);
        if !fields_dir.exists() {
            continue;
        }
        for f_entry in std::fs::read_dir(&fields_dir)? {
            let f_entry = f_entry?;
            let name = f_entry.file_name().to_string_lossy().to_string();
            if let Some(slug) = name.strip_suffix(".json") {
                if !slug.ends_with(".remote") {
                    out.push(slug.to_string());
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Auto-match engine slugs by directory name.
fn match_engines(
    existing: &mut BTreeMap<String, String>,
    src_paths: &Paths,
    tgt_paths: &Paths,
) -> Result<usize> {
    let src_slugs = list_engine_slugs(src_paths)?;
    let tgt_slugs: std::collections::HashSet<_> = list_engine_slugs(tgt_paths)?.into_iter().collect();
    let mut added = 0;
    for src_slug in &src_slugs {
        if existing.contains_key(src_slug) {
            continue;
        }
        if tgt_slugs.contains(src_slug) {
            existing.insert(src_slug.clone(), src_slug.clone());
            added += 1;
        }
    }
    Ok(added)
}

/// Auto-match engine-field slugs across all engines.
fn match_engine_fields(
    existing: &mut BTreeMap<String, String>,
    src_paths: &Paths,
    tgt_paths: &Paths,
) -> Result<usize> {
    let src_slugs = list_engine_field_slugs(src_paths)?;
    let tgt_slugs: std::collections::HashSet<_> = list_engine_field_slugs(tgt_paths)?.into_iter().collect();
    let mut added = 0;
    for src_slug in &src_slugs {
        if existing.contains_key(src_slug) {
            continue;
        }
        if tgt_slugs.contains(src_slug) {
            existing.insert(src_slug.clone(), src_slug.clone());
            added += 1;
        }
    }
    Ok(added)
}

/// Auto-match workspace slugs by directory name. Workspaces themselves are
/// pull-only on the deploy side (we don't PATCH a workspace's name across envs),
/// but their URLs are referenced by queues — so apply's URL rewriter needs the
/// mapping to translate `queue.workspace` from src URL to tgt URL.
fn match_workspaces(
    existing: &mut BTreeMap<String, String>,
    src_paths: &Paths,
    tgt_paths: &Paths,
) -> Result<usize> {
    let src_slugs = list_workspace_slugs(src_paths)?;
    let tgt_slugs: std::collections::HashSet<_> =
        list_workspace_slugs(tgt_paths)?.into_iter().collect();
    let mut added = 0;
    for src_slug in &src_slugs {
        if existing.contains_key(src_slug) {
            continue;
        }
        if tgt_slugs.contains(src_slug) {
            existing.insert(src_slug.clone(), src_slug.clone());
            added += 1;
        }
    }
    Ok(added)
}

fn list_workspace_slugs(paths: &Paths) -> Result<Vec<String>> {
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&workspaces_dir)
        .with_context(|| format!("reading {}", workspaces_dir.display()))?
    {
        let entry = entry.with_context(|| format!("listing {}", workspaces_dir.display()))?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let slug = entry.file_name().to_string_lossy().to_string();
        // Only count workspaces that have their JSON on disk.
        if entry.path().join("workspace.json").exists() {
            out.push(slug);
        }
    }
    out.sort();
    Ok(out)
}

/// Auto-match queue slugs by directory name. The lockfile keys queues by
/// `q_slug` only (not compound), so the mapping uses the same key.
fn match_queues(
    existing: &mut BTreeMap<String, String>,
    src_paths: &Paths,
    tgt_paths: &Paths,
) -> Result<usize> {
    let src_slugs = collect_queue_slugs(src_paths)?;
    let tgt_slugs: std::collections::HashSet<String> =
        collect_queue_slugs(tgt_paths)?.into_iter().collect();
    let mut added = 0;
    for q_slug in &src_slugs {
        if existing.contains_key(q_slug) {
            continue;
        }
        if tgt_slugs.contains(q_slug) {
            existing.insert(q_slug.clone(), q_slug.clone());
            added += 1;
        }
    }
    Ok(added)
}

/// Schemas are 1:1 with queues (lockfile keys them by queue slug). Auto-match
/// schemas wherever both src and tgt have a queue with the same slug AND a
/// schema.json present.
fn match_schemas(
    existing: &mut BTreeMap<String, String>,
    src_paths: &Paths,
    tgt_paths: &Paths,
) -> Result<usize> {
    match_queue_nested_file(existing, src_paths, tgt_paths, "schema.json")
}

fn match_inboxes(
    existing: &mut BTreeMap<String, String>,
    src_paths: &Paths,
    tgt_paths: &Paths,
) -> Result<usize> {
    match_queue_nested_file(existing, src_paths, tgt_paths, "inbox.json")
}

/// Walk every queue dir in both envs; if both src and tgt have `<file>` for
/// the same q_slug, register the mapping.
fn match_queue_nested_file(
    existing: &mut BTreeMap<String, String>,
    src_paths: &Paths,
    tgt_paths: &Paths,
    file: &str,
) -> Result<usize> {
    let src_slugs = collect_queue_slugs_with_file(src_paths, file)?;
    let tgt_slugs: std::collections::HashSet<String> =
        collect_queue_slugs_with_file(tgt_paths, file)?.into_iter().collect();
    let mut added = 0;
    for q_slug in &src_slugs {
        if existing.contains_key(q_slug) {
            continue;
        }
        if tgt_slugs.contains(q_slug) {
            existing.insert(q_slug.clone(), q_slug.clone());
            added += 1;
        }
    }
    Ok(added)
}

fn match_email_templates(
    existing: &mut BTreeMap<String, String>,
    src_paths: &Paths,
    tgt_paths: &Paths,
) -> Result<usize> {
    let src_keys = collect_email_template_keys(src_paths)?;
    let tgt_keys: std::collections::HashSet<String> =
        collect_email_template_keys(tgt_paths)?.into_iter().collect();
    let mut added = 0;
    for key in &src_keys {
        if existing.contains_key(key) {
            continue;
        }
        if tgt_keys.contains(key) {
            existing.insert(key.clone(), key.clone());
            added += 1;
        }
    }
    Ok(added)
}

fn collect_queue_slugs(paths: &Paths) -> Result<Vec<String>> {
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    for ws_entry in std::fs::read_dir(&workspaces_dir)
        .with_context(|| format!("reading {}", workspaces_dir.display()))?
    {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let ws_slug = ws_entry.file_name().to_string_lossy().to_string();
        let queues_dir = paths.queues_dir(&ws_slug);
        if !queues_dir.exists() {
            continue;
        }
        for q_entry in std::fs::read_dir(&queues_dir)
            .with_context(|| format!("reading {}", queues_dir.display()))?
        {
            let q_entry = q_entry?;
            if !q_entry.file_type()?.is_dir() {
                continue;
            }
            let q_slug = q_entry.file_name().to_string_lossy().to_string();
            // Only count dirs that actually have a queue.json (skip
            // partial-state dirs).
            if paths.queue_dir(&ws_slug, &q_slug).join("queue.json").exists() {
                out.push(q_slug);
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn collect_queue_slugs_with_file(paths: &Paths, file: &str) -> Result<Vec<String>> {
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    for ws_entry in std::fs::read_dir(&workspaces_dir)? {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let ws_slug = ws_entry.file_name().to_string_lossy().to_string();
        let queues_dir = paths.queues_dir(&ws_slug);
        if !queues_dir.exists() {
            continue;
        }
        for q_entry in std::fs::read_dir(&queues_dir)? {
            let q_entry = q_entry?;
            if !q_entry.file_type()?.is_dir() {
                continue;
            }
            let q_slug = q_entry.file_name().to_string_lossy().to_string();
            if paths.queue_dir(&ws_slug, &q_slug).join(file).exists() {
                out.push(q_slug);
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn collect_email_template_keys(paths: &Paths) -> Result<Vec<String>> {
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    for ws_entry in std::fs::read_dir(&workspaces_dir)? {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let ws_slug = ws_entry.file_name().to_string_lossy().to_string();
        let queues_dir = paths.queues_dir(&ws_slug);
        if !queues_dir.exists() {
            continue;
        }
        for q_entry in std::fs::read_dir(&queues_dir)? {
            let q_entry = q_entry?;
            if !q_entry.file_type()?.is_dir() {
                continue;
            }
            let q_slug = q_entry.file_name().to_string_lossy().to_string();
            let templates_dir = paths.queue_email_templates_dir(&ws_slug, &q_slug);
            if !templates_dir.exists() {
                continue;
            }
            for t_entry in std::fs::read_dir(&templates_dir)? {
                let t_entry = t_entry?;
                let name = t_entry.file_name().to_string_lossy().to_string();
                if let Some(template_slug) = name.strip_suffix(".json") {
                    if !template_slug.ends_with(".remote") {
                        out.push(format!("{ws_slug}/{q_slug}/{template_slug}"));
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}
