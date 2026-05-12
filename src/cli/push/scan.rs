//! Phase 1 of `rdc push`: walk the local snapshot, hash every writable file,
//! compare to lockfile, and produce a list of items needing PATCH per kind.
//! Phase 2 (the per-kind drivers) consumes this list — until Task 20 lands,
//! drivers still iterate the local tree themselves; the ChangeList is used
//! only for the early-exit "no changes" UX path.

use crate::paths::Paths;
use crate::state::Lockfile;
use anyhow::Result;
use std::collections::BTreeMap;

/// Items needing PATCH, grouped by kind. Slug is the key; the value is the
/// on-disk path so phase-2 drivers don't re-walk.
#[derive(Debug, Default)]
pub struct ChangeList {
    pub workspaces: BTreeMap<String, std::path::PathBuf>,
    pub hooks: BTreeMap<String, std::path::PathBuf>,
    pub rules: BTreeMap<String, std::path::PathBuf>,
    pub labels: BTreeMap<String, std::path::PathBuf>,
    pub queues: BTreeMap<String, std::path::PathBuf>,
    pub schemas: BTreeMap<String, std::path::PathBuf>,
    pub inboxes: BTreeMap<String, std::path::PathBuf>,
    pub email_templates: BTreeMap<String, std::path::PathBuf>,
    pub engines: BTreeMap<String, std::path::PathBuf>,
    pub engine_fields: BTreeMap<String, std::path::PathBuf>,
}

impl ChangeList {
    pub fn total(&self) -> usize {
        self.workspaces.len()
            + self.hooks.len()
            + self.rules.len()
            + self.labels.len()
            + self.queues.len()
            + self.schemas.len()
            + self.inboxes.len()
            + self.email_templates.len()
            + self.engines.len()
            + self.engine_fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// Walk the local snapshot, hash every writable file, compare to lockfile,
/// build a `ChangeList`. Returns `(scan_count, changes)`. The scan_count is
/// the total number of files inspected (not just changed ones).
pub fn scan(paths: &Paths, lockfile: &Lockfile) -> Result<(usize, ChangeList)> {
    let mut changes = ChangeList::default();
    let mut scanned = 0;

    scanned += scan_workspaces(paths, lockfile, &mut changes.workspaces)?;
    scanned += scan_hooks(paths, lockfile, &mut changes.hooks)?;
    scanned += scan_rules(paths, lockfile, &mut changes.rules)?;
    scanned += scan_flat_kind(paths, lockfile, "labels", paths.labels_dir(), &mut changes.labels)?;
    scanned += scan_queue_nested_json(paths, lockfile, "queues", "queue.json", &mut changes.queues)?;
    scanned += scan_schemas(paths, lockfile, &mut changes.schemas)?;
    scanned += scan_queue_nested_json(paths, lockfile, "inboxes", "inbox.json", &mut changes.inboxes)?;
    scanned += scan_email_templates(paths, lockfile, &mut changes.email_templates)?;
    scanned += scan_engines(paths, lockfile, &mut changes.engines)?;
    scanned += scan_engine_fields(paths, lockfile, &mut changes.engine_fields)?;

    Ok((scanned, changes))
}

/// Walk `engines/<slug>/engine.json` files. Mirrors `scan_workspaces`
/// since engines and workspaces share the dir-with-named-json shape.
fn scan_engines(
    paths: &Paths,
    lockfile: &Lockfile,
    out: &mut BTreeMap<String, std::path::PathBuf>,
) -> Result<usize> {
    use crate::state::content_hash;
    let engines_dir = paths.engines_dir();
    if !engines_dir.exists() {
        return Ok(0);
    }
    let mut scanned = 0;
    for e_entry in std::fs::read_dir(&engines_dir)? {
        let e_entry = e_entry?;
        if !e_entry.file_type()?.is_dir() {
            continue;
        }
        let e_slug = e_entry.file_name().to_string_lossy().to_string();
        let e_json_path = e_entry.path().join("engine.json");
        if !e_json_path.exists() {
            continue;
        }
        let bytes = std::fs::read(&e_json_path)?;
        let local_hash = content_hash(&bytes);
        scanned += 1;
        let base_hash = lockfile
            .objects
            .get("engines")
            .and_then(|m| m.get(&e_slug))
            .and_then(|x| x.content_hash.as_deref());
        if base_hash != Some(local_hash.as_str()) {
            out.insert(e_slug, e_json_path);
        }
    }
    Ok(scanned)
}

/// Walk `engines/<engine>/fields/<field>.json` files. Each field nests
/// under exactly one engine; the engine slug is just for path
/// resolution — the lockfile keys fields by field slug alone.
fn scan_engine_fields(
    paths: &Paths,
    lockfile: &Lockfile,
    out: &mut BTreeMap<String, std::path::PathBuf>,
) -> Result<usize> {
    use crate::state::content_hash;
    let engines_dir = paths.engines_dir();
    if !engines_dir.exists() {
        return Ok(0);
    }
    let mut scanned = 0;
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
            let f_path = f_entry.path();
            if f_path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(f_slug) = f_path.file_stem().and_then(|s| s.to_str()) else { continue };
            if f_slug.ends_with(".remote") {
                continue;
            }
            let bytes = std::fs::read(&f_path)?;
            let local_hash = content_hash(&bytes);
            scanned += 1;
            let base_hash = lockfile
                .objects
                .get("engine_fields")
                .and_then(|m| m.get(f_slug))
                .and_then(|x| x.content_hash.as_deref());
            if base_hash != Some(local_hash.as_str()) {
                out.insert(f_slug.to_string(), f_path);
            }
        }
    }
    Ok(scanned)
}

/// Walk `rules/<slug>.json` files. Each rule may have a sibling
/// `<slug>.py` carrying the extracted `trigger_condition`; the
/// combined hash covers both. Mirrors `scan_hooks`.
fn scan_rules(
    paths: &Paths,
    lockfile: &Lockfile,
    out: &mut BTreeMap<String, std::path::PathBuf>,
) -> Result<usize> {
    use crate::state::rule_combined_hash;
    let dir = paths.rules_dir();
    if !dir.exists() {
        return Ok(0);
    }
    let mut scanned = 0;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        if slug.ends_with(".remote") {
            continue;
        }
        let json_bytes = std::fs::read(&path)?;
        let py_path = path.with_extension("py");
        let code = if py_path.exists() {
            Some(std::fs::read_to_string(&py_path)?)
        } else {
            None
        };
        let local_hash = rule_combined_hash(&json_bytes, &code);
        scanned += 1;
        let base_hash = lockfile
            .objects
            .get("rules")
            .and_then(|m| m.get(slug))
            .and_then(|e| e.content_hash.as_deref());
        if base_hash != Some(local_hash.as_str()) {
            out.insert(slug.to_string(), path);
        }
    }
    Ok(scanned)
}

fn scan_workspaces(
    paths: &Paths,
    lockfile: &Lockfile,
    out: &mut BTreeMap<String, std::path::PathBuf>,
) -> Result<usize> {
    use crate::state::content_hash;
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return Ok(0);
    }
    let mut scanned = 0;
    for ws_entry in std::fs::read_dir(&workspaces_dir)? {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let ws_slug = ws_entry.file_name().to_string_lossy().to_string();
        let ws_json_path = ws_entry.path().join("workspace.json");
        if !ws_json_path.exists() {
            continue;
        }
        let bytes = std::fs::read(&ws_json_path)?;
        let local_hash = content_hash(&bytes);
        scanned += 1;
        let base_hash = lockfile
            .objects
            .get("workspaces")
            .and_then(|m| m.get(&ws_slug))
            .and_then(|e| e.content_hash.as_deref());
        if base_hash != Some(local_hash.as_str()) {
            out.insert(ws_slug, ws_json_path);
        }
    }
    Ok(scanned)
}

fn scan_hooks(
    paths: &Paths,
    lockfile: &Lockfile,
    out: &mut BTreeMap<String, std::path::PathBuf>,
) -> Result<usize> {
    use crate::state::hook_combined_hash;
    let dir = paths.hooks_dir();
    if !dir.exists() {
        return Ok(0);
    }
    let mut scanned = 0;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        // Skip .remote files (e.g. "validator-invoices.remote.json")
        if slug.ends_with(".remote") {
            continue;
        }
        let json_bytes = std::fs::read(&path)?;
        let py_path = path.with_extension("py");
        let code = if py_path.exists() {
            Some(std::fs::read_to_string(&py_path)?)
        } else {
            None
        };
        let local_hash = hook_combined_hash(&json_bytes, &code);
        scanned += 1;
        let base_hash = lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(slug))
            .and_then(|e| e.content_hash.as_deref());
        if base_hash != Some(local_hash.as_str()) {
            out.insert(slug.to_string(), path);
        }
    }
    Ok(scanned)
}

fn scan_flat_kind(
    _paths: &Paths,
    lockfile: &Lockfile,
    kind: &str,
    dir: std::path::PathBuf,
    out: &mut BTreeMap<String, std::path::PathBuf>,
) -> Result<usize> {
    use crate::state::content_hash;
    if !dir.exists() {
        return Ok(0);
    }
    let mut scanned = 0;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let bytes = std::fs::read(&path)?;
        let local_hash = content_hash(&bytes);
        scanned += 1;
        let base_hash = lockfile
            .objects
            .get(kind)
            .and_then(|m| m.get(slug))
            .and_then(|e| e.content_hash.as_deref());
        if base_hash != Some(local_hash.as_str()) {
            out.insert(slug.to_string(), path);
        }
    }
    Ok(scanned)
}

fn scan_queue_nested_json(
    paths: &Paths,
    lockfile: &Lockfile,
    kind: &str,
    filename: &str,
    out: &mut BTreeMap<String, std::path::PathBuf>,
) -> Result<usize> {
    use crate::state::content_hash;
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return Ok(0);
    }
    let mut scanned = 0;
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
            let q_path = q_entry.path();
            let target = q_path.join(filename);
            if !target.exists() {
                continue;
            }
            let Some(q_slug) = q_path.file_name().and_then(|s| s.to_str()) else { continue };
            let bytes = std::fs::read(&target)?;
            let local_hash = content_hash(&bytes);
            scanned += 1;
            let base_hash = lockfile
                .objects
                .get(kind)
                .and_then(|m| m.get(q_slug))
                .and_then(|e| e.content_hash.as_deref());
            if base_hash != Some(local_hash.as_str()) {
                out.insert(q_slug.to_string(), target);
            }
        }
    }
    Ok(scanned)
}

fn scan_schemas(
    paths: &Paths,
    lockfile: &Lockfile,
    out: &mut BTreeMap<String, std::path::PathBuf>,
) -> Result<usize> {
    use crate::snapshot::schema::read_local_formulas;
    use crate::state::schema_combined_hash;
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return Ok(0);
    }
    let mut scanned = 0;
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
            let q_path = q_entry.path();
            let schema_path = q_path.join("schema.json");
            if !schema_path.exists() {
                continue;
            }
            let Some(q_slug) = q_path.file_name().and_then(|s| s.to_str()) else { continue };
            let json_bytes = std::fs::read(&schema_path)?;
            let formulas = read_local_formulas(&q_path).unwrap_or_default();
            let local_hash = schema_combined_hash(&json_bytes, &formulas);
            scanned += 1;
            let base_hash = lockfile
                .objects
                .get("schemas")
                .and_then(|m| m.get(q_slug))
                .and_then(|e| e.content_hash.as_deref());
            if base_hash != Some(local_hash.as_str()) {
                out.insert(q_slug.to_string(), schema_path);
            }
        }
    }
    Ok(scanned)
}

fn scan_email_templates(
    paths: &Paths,
    lockfile: &Lockfile,
    out: &mut BTreeMap<String, std::path::PathBuf>,
) -> Result<usize> {
    use crate::state::content_hash;
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return Ok(0);
    }
    let mut scanned = 0;
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
            let templates_dir = q_entry.path().join("email-templates");
            if !templates_dir.exists() {
                continue;
            }
            for t_entry in std::fs::read_dir(&templates_dir)? {
                let t_entry = t_entry?;
                let t_path = t_entry.path();
                if t_path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let Some(t_slug) = t_path.file_stem().and_then(|s| s.to_str()) else { continue };
                let compound = format!("{ws_slug}/{q_slug}/{t_slug}");
                let bytes = std::fs::read(&t_path)?;
                let local_hash = content_hash(&bytes);
                scanned += 1;
                let base_hash = lockfile
                    .objects
                    .get("email_templates")
                    .and_then(|m| m.get(&compound))
                    .and_then(|e| e.content_hash.as_deref());
                if base_hash != Some(local_hash.as_str()) {
                    out.insert(compound, t_path);
                }
            }
        }
    }
    Ok(scanned)
}
