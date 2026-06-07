//! Phase 1 of `rdc push`: walk the local snapshot, hash every writable file,
//! compare to lockfile, and produce a list of items needing PATCH per kind.
//! Phase 2 (the per-kind drivers) consumes this list — until Task 20 lands,
//! drivers still iterate the local tree themselves; the ChangeList is used
//! only for the early-exit "no changes" UX path.
//!
//! The scan also reports **tombstones**: lockfile entries whose on-disk
//! file is missing. These are the user's explicit "delete this from
//! remote" signal — see `Tombstones` below.

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

/// Lockfile entries whose on-disk file is missing — the user's explicit
/// "delete this from remote" signal. Each entry stores the lockfile-known
/// remote `id` so the push driver can issue `DELETE /<kind>/<id>` without
/// re-reading the lockfile.
#[derive(Debug, Default)]
pub struct Tombstones {
    pub workspaces: BTreeMap<String, u64>,
    pub hooks: BTreeMap<String, u64>,
    pub rules: BTreeMap<String, u64>,
    pub labels: BTreeMap<String, u64>,
    pub queues: BTreeMap<String, u64>,
    pub schemas: BTreeMap<String, u64>,
    pub inboxes: BTreeMap<String, u64>,
    pub email_templates: BTreeMap<String, u64>,
    pub engines: BTreeMap<String, u64>,
    pub engine_fields: BTreeMap<String, u64>,
}

impl Tombstones {
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
/// build a `ChangeList` for POST/PATCH candidates and a `Tombstones` list
/// for the lockfile entries whose local file is missing. Returns
/// `(scan_count, changes, tombstones)`.
pub fn scan(paths: &Paths, lockfile: &Lockfile) -> Result<(usize, ChangeList, Tombstones)> {
    let mut changes = ChangeList::default();
    let mut scanned = 0;

    scanned += scan_workspaces(paths, lockfile, &mut changes.workspaces)?;
    scanned += scan_hooks(paths, lockfile, &mut changes.hooks)?;
    scanned += scan_rules(paths, lockfile, &mut changes.rules)?;
    scanned += scan_flat_kind(
        paths,
        lockfile,
        "labels",
        paths.labels_dir(),
        &mut changes.labels,
    )?;
    scanned +=
        scan_queue_nested_json(paths, lockfile, "queues", "queue.json", &mut changes.queues)?;
    scanned += scan_schemas(paths, lockfile, &mut changes.schemas)?;
    scanned += scan_queue_nested_json(
        paths,
        lockfile,
        "inboxes",
        "inbox.json",
        &mut changes.inboxes,
    )?;
    scanned += scan_email_templates(paths, lockfile, &mut changes.email_templates)?;
    scanned += scan_engines(paths, lockfile, &mut changes.engines)?;
    scanned += scan_engine_fields(paths, lockfile, &mut changes.engine_fields)?;

    let tombstones = detect_tombstones(paths, lockfile);

    Ok((scanned, changes, tombstones))
}

/// Cross-check the lockfile against the local snapshot: every lockfile
/// entry without a corresponding on-disk file becomes a tombstone.
///
/// Each kind has its own expected file location (workspace.json lives in
/// `workspaces/<slug>/`, schemas in `workspaces/<ws>/queues/<slug>/`,
/// email_templates use a compound key, etc.). For the queue-nested kinds
/// we don't know the workspace from the lockfile entry, so we sweep every
/// `workspaces/*/queues/<slug>/<file>` and treat the slug as tombstoned
/// only if no workspace contains it.
///
/// Exposed `pub` so sync can surface tombstones in its summary
/// without re-running the full scan.
pub fn detect_tombstones(paths: &Paths, lockfile: &Lockfile) -> Tombstones {
    let mut t = Tombstones::default();

    // --- flat kinds: <kind>/<slug>.json -----------------------------
    detect_flat(lockfile, "hooks", &paths.hooks_dir(), &mut t.hooks);
    detect_flat(lockfile, "rules", &paths.rules_dir(), &mut t.rules);
    detect_flat(lockfile, "labels", &paths.labels_dir(), &mut t.labels);

    // --- workspaces: workspaces/<slug>/workspace.json ---------------
    if let Some(map) = lockfile.objects.get("workspaces") {
        for (slug, entry) in map {
            let path = paths.workspace_dir(slug).join("workspace.json");
            if !path.exists() {
                t.workspaces.insert(slug.clone(), entry.id);
            }
        }
    }

    // --- queue-nested: workspaces/<ws>/queues/<slug>/<file> ---------
    detect_queue_nested(paths, lockfile, "queues", "queue.json", &mut t.queues);
    detect_queue_nested(paths, lockfile, "schemas", "schema.json", &mut t.schemas);
    detect_queue_nested(paths, lockfile, "inboxes", "inbox.json", &mut t.inboxes);

    // --- email_templates: compound key "<ws>/<q>/<template>" --------
    if let Some(map) = lockfile.objects.get("email_templates") {
        for (key, entry) in map {
            let parts: Vec<&str> = key.splitn(3, '/').collect();
            if parts.len() == 3 {
                let path = paths
                    .queue_email_templates_dir(parts[0], parts[1])
                    .join(format!("{}.json", parts[2]));
                if !path.exists() {
                    t.email_templates.insert(key.clone(), entry.id);
                }
            }
        }
    }

    // --- engines: engines/<slug>/engine.json -------------------------
    if let Some(map) = lockfile.objects.get("engines") {
        for (slug, entry) in map {
            let path = paths.engines_dir().join(slug).join("engine.json");
            if !path.exists() {
                t.engines.insert(slug.clone(), entry.id);
            }
        }
    }

    // --- engine_fields: engines/<engine>/fields/<slug>.json ----------
    if let Some(map) = lockfile.objects.get("engine_fields") {
        for (slug, entry) in map {
            if !engine_field_file_exists(paths, slug) {
                t.engine_fields.insert(slug.clone(), entry.id);
            }
        }
    }

    t
}

fn detect_flat(
    lockfile: &Lockfile,
    kind: &str,
    dir: &std::path::Path,
    out: &mut BTreeMap<String, u64>,
) {
    let Some(map) = lockfile.objects.get(kind) else {
        return;
    };
    for (slug, entry) in map {
        let path = dir.join(format!("{slug}.json"));
        if !path.exists() {
            out.insert(slug.clone(), entry.id);
        }
    }
}

fn detect_queue_nested(
    paths: &Paths,
    lockfile: &Lockfile,
    kind: &str,
    file_name: &str,
    out: &mut BTreeMap<String, u64>,
) {
    let Some(map) = lockfile.objects.get(kind) else {
        return;
    };
    for (slug, entry) in map {
        if !queue_nested_file_exists(paths, slug, file_name) {
            out.insert(slug.clone(), entry.id);
        }
    }
}

fn queue_nested_file_exists(paths: &Paths, q_slug: &str, file_name: &str) -> bool {
    let ws_dir = paths.workspaces_dir();
    if !ws_dir.exists() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(&ws_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if entry
            .path()
            .join("queues")
            .join(q_slug)
            .join(file_name)
            .exists()
        {
            return true;
        }
    }
    false
}

fn engine_field_file_exists(paths: &Paths, composite_key: &str) -> bool {
    // `composite_key` is `<engine_slug>/<field_slug>`; the file lives at
    // `engines/<engine>/fields/<field>.json`. Fall back to a global walk
    // when the key isn't composite (legacy flat lockfile entry that
    // hasn't migrated yet).
    if let Some((e_slug, f_slug)) = composite_key.split_once('/') {
        return paths
            .engine_fields_dir(e_slug)
            .join(format!("{f_slug}.json"))
            .exists();
    }
    let engines_dir = paths.engines_dir();
    if !engines_dir.exists() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(&engines_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if entry
            .path()
            .join("fields")
            .join(format!("{composite_key}.json"))
            .exists()
        {
            return true;
        }
    }
    false
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
        let local_hash = content_hash(&bytes, &crate::state::Lockfile::default());
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
/// under exactly one engine; the lockfile keys fields by the composite
/// `<engine_slug>/<field_slug>` so two engines can both carry a field
/// with the same field-slug (and thus the same `.json` filename).
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
            let name = f_entry.file_name().to_string_lossy().to_string();
            // Skip env-named shadow artifacts.
            if crate::paths::is_shadow_artifact(&name, paths.env()) {
                continue;
            }
            if f_path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(f_slug) = f_path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let composite_key = format!("{e_slug}/{f_slug}");
            let bytes = std::fs::read(&f_path)?;
            let local_hash = content_hash(&bytes, &crate::state::Lockfile::default());
            scanned += 1;
            let base_hash = lockfile
                .objects
                .get("engine_fields")
                .and_then(|m| {
                    // Prefer composite key; fall back to legacy flat key
                    // so a not-yet-migrated lockfile still classifies
                    // correctly during the first sync after upgrade.
                    m.get(&composite_key).or_else(|| m.get(f_slug))
                })
                .and_then(|x| x.content_hash.as_deref());
            if base_hash != Some(local_hash.as_str()) {
                out.insert(composite_key, f_path);
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
        let name = entry.file_name().to_string_lossy().to_string();
        // Skip env-named shadow artifacts.
        if crate::paths::is_shadow_artifact(&name, paths.env()) {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let json_bytes = std::fs::read(&path)?;
        let py_path = path.with_extension("py");
        let code = if py_path.exists() {
            Some(std::fs::read_to_string(&py_path)?)
        } else {
            None
        };
        let local_hash = rule_combined_hash(&json_bytes, &code, &crate::state::Lockfile::default());
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
        let local_hash = content_hash(&bytes, &crate::state::Lockfile::default());
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
    use crate::snapshot::hook::hook_code_extension_from_value;
    use crate::state::hook_combined_hash;
    let dir = paths.hooks_dir();
    if !dir.exists() {
        return Ok(0);
    }
    let mut scanned = 0;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // Skip env-named shadow artifacts (e.g. "validator-invoices.json.dev").
        if crate::paths::is_shadow_artifact(&name, paths.env()) {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let json_bytes = std::fs::read(&path)?;
        // Sidecar extension derives from the JSON's `config.runtime`:
        // `.js` for Node.js runtimes, `.py` otherwise. Fall back to the
        // other extension if the runtime-derived one is missing
        // (defensive — handles runtime-changed-but-sidecar-stale).
        let value: serde_json::Value = match serde_json::from_slice(&json_bytes) {
            Ok(v) => v,
            // If the JSON doesn't parse we leave `ext` at the default
            // and let downstream errors surface elsewhere.
            Err(_) => serde_json::Value::Null,
        };
        let ext = hook_code_extension_from_value(&value);
        let primary = path.with_extension(ext);
        let fallback = path.with_extension(if ext == "py" { "js" } else { "py" });
        let code = if primary.exists() {
            Some(std::fs::read_to_string(&primary)?)
        } else if fallback.exists() {
            Some(std::fs::read_to_string(&fallback)?)
        } else {
            None
        };
        let local_hash = hook_combined_hash(&json_bytes, &code, &crate::state::Lockfile::default());
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
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let bytes = std::fs::read(&path)?;
        let local_hash = content_hash(&bytes, &crate::state::Lockfile::default());
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
            let Some(q_slug) = q_path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let bytes = std::fs::read(&target)?;
            let local_hash = content_hash(&bytes, &crate::state::Lockfile::default());
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
            let Some(q_slug) = q_path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let json_bytes = std::fs::read(&schema_path)?;
            let formulas = read_local_formulas(&q_path).unwrap_or_default();
            let local_hash =
                schema_combined_hash(&json_bytes, &formulas, &crate::state::Lockfile::default());
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
                let Some(t_slug) = t_path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let compound = format!("{ws_slug}/{q_slug}/{t_slug}");
                let bytes = std::fs::read(&t_path)?;
                let local_hash = content_hash(&bytes, &crate::state::Lockfile::default());
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

/// Convert a list of classified items (from `cli::sync::classify`) into a
/// push-side `ChangeList`. Only `LocalEdit` and `LocalCreate` items are
/// retained — those are the classes the push pipeline knows how to PATCH /
/// POST. `LocalDelete` is handled separately via tombstones; all remote-side
/// classes and `Clean` are silently dropped.
///
/// The classified items carry only `(kind, slug)`, so for each push-side
/// item this helper computes the on-disk path via the same layout the
/// `scan` walkers use. For flat kinds (`hooks`, `rules`, `labels`, etc.)
/// the path is built directly from the slug; for queue-nested kinds
/// (`queues`, `schemas`, `inboxes`) the lockfile keys items by queue slug
/// alone, so we sweep `workspaces/*/queues/<slug>/<file>` to find the
/// owning workspace. For `email_templates` the slug is already the
/// `<ws>/<queue>/<template>` compound, so the split is unambiguous. For
/// `engine_fields` we sweep `engines/*/fields/<slug>.json` (lockfile keys
/// fields by field slug alone, same as the existing scanner). Kinds that
/// don't go through the push pipeline (`mdh`, `workflows`,
/// `workflow_steps`, `organization`) are silently dropped.
pub fn change_list_from_classified(
    paths: &crate::paths::Paths,
    items: &[crate::cli::sync::classify::ClassifiedItem],
) -> ChangeList {
    use crate::cli::sync::classify::SyncClass;
    let mut cl = ChangeList::default();
    for it in items {
        if !matches!(it.class, SyncClass::LocalEdit | SyncClass::LocalCreate) {
            continue;
        }
        match it.kind.as_str() {
            "workspaces" => {
                cl.workspaces.insert(
                    it.slug.clone(),
                    paths.workspace_dir(&it.slug).join("workspace.json"),
                );
            }
            "hooks" => {
                cl.hooks.insert(
                    it.slug.clone(),
                    paths.hooks_dir().join(format!("{}.json", it.slug)),
                );
            }
            "rules" => {
                cl.rules.insert(
                    it.slug.clone(),
                    paths.rules_dir().join(format!("{}.json", it.slug)),
                );
            }
            "labels" => {
                cl.labels.insert(
                    it.slug.clone(),
                    paths.labels_dir().join(format!("{}.json", it.slug)),
                );
            }
            "engines" => {
                cl.engines.insert(
                    it.slug.clone(),
                    paths.engine_dir(&it.slug).join("engine.json"),
                );
            }
            "engine_fields" => {
                if let Some(p) = find_engine_field_path(paths, &it.slug) {
                    cl.engine_fields.insert(it.slug.clone(), p);
                }
            }
            "queues" => {
                if let Some(p) = find_queue_nested_path(paths, &it.slug, "queue.json") {
                    cl.queues.insert(it.slug.clone(), p);
                }
            }
            "schemas" => {
                if let Some(p) = find_queue_nested_path(paths, &it.slug, "schema.json") {
                    cl.schemas.insert(it.slug.clone(), p);
                }
            }
            "inboxes" => {
                if let Some(p) = find_queue_nested_path(paths, &it.slug, "inbox.json") {
                    cl.inboxes.insert(it.slug.clone(), p);
                }
            }
            "email_templates" => {
                // Compound key "<ws>/<queue>/<template>".
                let parts: Vec<&str> = it.slug.splitn(3, '/').collect();
                if parts.len() == 3 {
                    let p = paths
                        .queue_email_templates_dir(parts[0], parts[1])
                        .join(format!("{}.json", parts[2]));
                    cl.email_templates.insert(it.slug.clone(), p);
                }
            }
            // Other kinds (mdh, workflows, workflow_steps, organization) don't
            // go through the push pipeline; silently drop. Workflows and
            // workflow_steps are read-only at the Rossum API; organization is
            // singleton-read.
            _ => {}
        }
    }
    cl
}

/// Sweep `workspaces/*/queues/<q_slug>/<file_name>` and return the first
/// match. Mirrors `queue_nested_file_exists` but returns the path. Used by
/// `change_list_from_classified` for `queues` / `schemas` / `inboxes`,
/// whose classifier keys items by queue slug alone. Also used by the
/// sync executor's remote-delete dispatcher for the same kinds.
pub(crate) fn find_queue_nested_path(
    paths: &Paths,
    q_slug: &str,
    file_name: &str,
) -> Option<std::path::PathBuf> {
    let ws_dir = paths.workspaces_dir();
    if !ws_dir.exists() {
        return None;
    }
    let entries = std::fs::read_dir(&ws_dir).ok()?;
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let candidate = entry.path().join("queues").join(q_slug).join(file_name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve the on-disk path for an engine_field given its composite key
/// `<engine_slug>/<field_slug>`. Falls back to a global `engines/*/fields/`
/// sweep for legacy flat keys (lockfile entries written before the
/// composite-key migration).
fn find_engine_field_path(paths: &Paths, composite_key: &str) -> Option<std::path::PathBuf> {
    if let Some((e_slug, f_slug)) = composite_key.split_once('/') {
        let candidate = paths
            .engine_fields_dir(e_slug)
            .join(format!("{f_slug}.json"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
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
            .join(format!("{composite_key}.json"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_list_from_classified_groups_push_side_items_by_kind() {
        use crate::cli::sync::classify::{ClassifiedItem, SyncClass};
        use crate::paths::Paths;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");

        let items = vec![
            ClassifiedItem {
                kind: "hooks".into(),
                slug: "h1".into(),
                class: SyncClass::LocalEdit,
                local_hash: Some("h".into()),
                remote_hash: Some("h".into()),
                base_hash: Some("h".into()),
            },
            ClassifiedItem {
                kind: "labels".into(),
                slug: "l1".into(),
                class: SyncClass::LocalCreate,
                local_hash: Some("h".into()),
                remote_hash: None,
                base_hash: None,
            },
            ClassifiedItem {
                kind: "hooks".into(),
                slug: "h2".into(),
                // Not a push-side class — should be ignored.
                class: SyncClass::RemoteEdit,
                local_hash: Some("h".into()),
                remote_hash: Some("h2".into()),
                base_hash: Some("h".into()),
            },
        ];

        let cl = change_list_from_classified(&paths, &items);
        assert_eq!(
            cl.hooks.len(),
            1,
            "only the LocalEdit hook should be in the list"
        );
        assert!(cl.hooks.contains_key("h1"));
        assert_eq!(cl.labels.len(), 1);
        assert!(cl.labels.contains_key("l1"));
        assert!(cl.queues.is_empty());
    }
}
