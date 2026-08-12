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

    /// Validate that every changed local JSON file actually parses.
    /// The scanner itself hashes raw bytes (hashing must never fail), so
    /// a malformed file otherwise rides through classification as a
    /// normal local edit and only explodes mid-push — after earlier
    /// kinds were already written. Callers surface these in the dry-run
    /// plan and refuse a real push before the first remote write.
    pub fn json_parse_errors(&self) -> Vec<JsonParseError> {
        let mut out = Vec::new();
        let mut check = |kind: &'static str, map: &BTreeMap<String, std::path::PathBuf>| {
            for (slug, path) in map {
                let Ok(bytes) = std::fs::read(path) else {
                    continue; // unreadable ≠ unparseable; push surfaces I/O errors
                };
                if let Err(e) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    out.push(JsonParseError {
                        kind,
                        slug: slug.clone(),
                        path: path.clone(),
                        error: e.to_string(),
                    });
                }
            }
        };
        check("workspaces", &self.workspaces);
        check("hooks", &self.hooks);
        check("rules", &self.rules);
        check("labels", &self.labels);
        check("queues", &self.queues);
        check("schemas", &self.schemas);
        check("inboxes", &self.inboxes);
        check("email_templates", &self.email_templates);
        check("engines", &self.engines);
        check("engine_fields", &self.engine_fields);
        out
    }

    /// Validate every changed local file against the Rossum API's
    /// declared `max_length` limits (see [`crate::snapshot::limits`]).
    ///
    /// An oversized field is a *permanent* push failure: the server
    /// answers `400` and no retry can ever succeed while the local bytes
    /// stay too long. Since the push phase precedes the pull phase and
    /// its error aborts the cycle, one oversized field otherwise wedges
    /// the entire project — every later `rdc sync` dies at the same
    /// PATCH and no pull lands again. Callers surface these in the
    /// dry-run plan and refuse a real push before the first remote
    /// write, exactly like [`ChangeList::json_parse_errors`].
    ///
    /// Each file is stripped with `strip_for_create` before checking, so
    /// a long value in a server-managed field (which never reaches the
    /// wire) can't produce a false positive.
    pub fn field_limit_violations(&self) -> Vec<FieldLimitViolation> {
        let mut out = Vec::new();
        let mut check = |kind: &'static str, map: &BTreeMap<String, std::path::PathBuf>| {
            for (slug, path) in map {
                let Ok(bytes) = std::fs::read(path) else {
                    continue; // unreadable — push surfaces I/O errors
                };
                let Ok(mut body) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                    continue; // unparseable — reported by json_parse_errors
                };
                crate::snapshot::create::strip_for_create(&mut body, kind);
                for v in crate::snapshot::limits::check_field_limits(kind, &body) {
                    out.push(FieldLimitViolation {
                        kind,
                        slug: slug.clone(),
                        path: path.clone(),
                        field: v.field,
                        limit: v.limit,
                        actual: v.actual,
                    });
                }
            }
        };
        check("workspaces", &self.workspaces);
        check("hooks", &self.hooks);
        check("rules", &self.rules);
        check("labels", &self.labels);
        check("queues", &self.queues);
        check("schemas", &self.schemas);
        check("inboxes", &self.inboxes);
        check("email_templates", &self.email_templates);
        check("engines", &self.engines);
        check("engine_fields", &self.engine_fields);

        // `rules.trigger_condition` lives in `<slug>.py`, never in the JSON,
        // so the JSON walk above can never see it. Point the violation at
        // the sidecar — that is the file the user opens to fix it.
        for (slug, json_path) in &self.rules {
            let py_path = json_path.with_extension("py");
            let Ok(text) = std::fs::read_to_string(&py_path) else {
                continue; // no trigger_condition, or unreadable — push surfaces I/O errors
            };
            if let Some(v) = crate::snapshot::limits::check_text(
                "trigger_condition",
                crate::snapshot::limits::RULE_TRIGGER_CONDITION_LIMIT,
                &text,
            ) {
                out.push(FieldLimitViolation {
                    kind: "rules",
                    slug: slug.clone(),
                    path: py_path,
                    field: v.field,
                    limit: v.limit,
                    actual: v.actual,
                });
            }
        }

        // Schema formulas live in `<queue_dir>/formulas/<id>.py` and are
        // spliced back into the body on push. `ChangeList.schemas` stores the
        // path to `schema.json`, so the queue dir is its parent.
        for (slug, schema_path) in &self.schemas {
            let Some(queue_dir) = schema_path.parent() else {
                continue;
            };
            let formulas =
                crate::snapshot::schema::read_local_formulas(queue_dir).unwrap_or_default();
            for (id, bytes) in formulas {
                let Ok(text) = String::from_utf8(bytes) else {
                    continue; // not UTF-8 — the push path surfaces that
                };
                if let Some(v) = crate::snapshot::limits::check_text(
                    format!("formula on datapoint '{id}'"),
                    crate::snapshot::limits::SCHEMA_FORMULA_LIMIT,
                    &text,
                ) {
                    out.push(FieldLimitViolation {
                        kind: "schemas",
                        slug: slug.clone(),
                        path: queue_dir.join("formulas").join(format!("{id}.py")),
                        field: v.field,
                        limit: v.limit,
                        actual: v.actual,
                    });
                }
            }
        }

        out
    }
}

/// One changed local file whose field exceeds the API's `max_length`, as
/// reported by [`ChangeList::field_limit_violations`].
#[derive(Debug)]
pub struct FieldLimitViolation {
    pub kind: &'static str,
    pub slug: String,
    pub path: std::path::PathBuf,
    /// Where the offending value lives: usually a top-level JSON key
    /// (`description`), but for a sidecar-extracted field like a rule's
    /// `trigger_condition` this names the field even though it is never a
    /// JSON key at all — see `ChangeList::field_limit_violations`.
    pub field: String,
    /// The API's declared limit for this field.
    pub limit: usize,
    /// The local value's length, in characters.
    pub actual: usize,
}

/// One unparseable changed local file, as reported by
/// [`ChangeList::json_parse_errors`].
pub struct JsonParseError {
    pub kind: &'static str,
    pub slug: String,
    pub path: std::path::PathBuf,
    pub error: String,
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

/// Detect queue-slug collisions: the same queue directory name under more than
/// one workspace. Queue / schema / inbox lockfile entries are keyed by this
/// slug ALONE, so a collision collapses two distinct queues onto one lockfile
/// entry — [`scan`]'s per-kind `BTreeMap` (keyed by slug) silently keeps one,
/// and the other is classified as changed on every sync, pushed forever, its
/// base clobbering the winner's (a permanent non-idempotency). A fresh pull
/// assigns globally-unique slugs, so a collision means the on-disk snapshot
/// predates that dedup and should be re-pulled. Returns `slug -> sorted
/// workspace slugs` only for slugs present in more than one workspace.
pub fn detect_slug_collisions(paths: &Paths) -> BTreeMap<String, Vec<String>> {
    let mut by_slug: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let Ok(ws_entries) = std::fs::read_dir(paths.workspaces_dir()) else {
        return BTreeMap::new();
    };
    for ws in ws_entries.flatten() {
        if !ws.path().is_dir() {
            continue;
        }
        let ws_slug = ws.file_name().to_string_lossy().to_string();
        let Ok(q_entries) = std::fs::read_dir(paths.queues_dir(&ws_slug)) else {
            continue;
        };
        for q in q_entries.flatten() {
            // Only real queues (a queue.json present) count — partial-state
            // dirs are not lockfile-keyed and can't collide.
            if q.path().join("queue.json").is_file() {
                let slug = q.file_name().to_string_lossy().to_string();
                by_slug.entry(slug).or_default().push(ws_slug.clone());
            }
        }
    }
    by_slug.retain(|_, wss| {
        wss.sort();
        wss.dedup();
        wss.len() > 1
    });
    by_slug
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An oversized field in a changed local file must be reported with
    /// enough context to fix it: kind, slug, path, field, actual, limit.
    #[test]
    fn field_limit_violations_reports_oversized_hook_description() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("example-hook.json");
        std::fs::write(
            &p,
            serde_json::to_vec(&serde_json::json!({ "description": "x".repeat(2406) })).unwrap(),
        )
        .unwrap();
        let mut cl = ChangeList::default();
        cl.hooks.insert("example-hook".to_string(), p.clone());

        let v = cl.field_limit_violations();
        assert_eq!(v.len(), 1, "expected exactly one violation: {v:?}");
        assert_eq!(v[0].kind, "hooks");
        assert_eq!(v[0].slug, "example-hook");
        assert_eq!(v[0].path, p);
        assert_eq!(v[0].field, "description");
        assert_eq!(v[0].actual, 2406);
        assert_eq!(v[0].limit, 2000);
    }

    /// A within-limit file must produce nothing — this check must never
    /// block a legitimate push.
    #[test]
    fn field_limit_violations_ignores_within_limit_values() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("example-hook.json");
        std::fs::write(
            &p,
            serde_json::to_vec(&serde_json::json!({ "description": "x".repeat(2000) })).unwrap(),
        )
        .unwrap();
        let mut cl = ChangeList::default();
        cl.hooks.insert("example-hook".to_string(), p);
        assert_eq!(cl.field_limit_violations().len(), 0);
    }

    /// Unparseable JSON is already reported by `json_parse_errors`; the
    /// limit check must skip it rather than double-reporting or panicking.
    #[test]
    fn field_limit_violations_skips_unparseable_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("broken.json");
        std::fs::write(&p, b"{not json").unwrap();
        let mut cl = ChangeList::default();
        cl.hooks.insert("broken".to_string(), p);
        assert_eq!(cl.field_limit_violations().len(), 0);
    }

    /// Server-managed fields are stripped from the outgoing body, so an
    /// oversized value there is never sent and must not block the push.
    #[test]
    fn field_limit_violations_ignores_fields_stripped_before_push() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("example-hook.json");
        std::fs::write(
            &p,
            serde_json::to_vec(&serde_json::json!({
                "url": "https://example.test/".to_string() + &"u".repeat(4000),
                "description": "fine",
            }))
            .unwrap(),
        )
        .unwrap();
        let mut cl = ChangeList::default();
        cl.hooks.insert("example-hook".to_string(), p);
        assert_eq!(cl.field_limit_violations().len(), 0);
    }

    #[test]
    fn detect_slug_collisions_finds_cross_workspace_dupes() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        // Two workspaces each with a queue dir named "invoices" (collision),
        // plus a unique "credit-memos" in one — only the collision is reported.
        for (ws, q) in [
            ("main", "invoices"),
            ("phase-1", "invoices"),
            ("main", "credit-memos"),
        ] {
            let qd = paths.queue_dir(ws, q);
            std::fs::create_dir_all(&qd).unwrap();
            std::fs::write(qd.join("queue.json"), b"{}").unwrap();
        }
        let cols = detect_slug_collisions(&paths);
        assert_eq!(cols.len(), 1, "only the colliding slug is reported: {cols:?}");
        assert_eq!(cols.get("invoices").unwrap(), &vec!["main".to_string(), "phase-1".to_string()]);
        assert!(!cols.contains_key("credit-memos"), "unique slug must not be reported");
    }

    #[test]
    fn detect_slug_collisions_ignores_dirs_without_queue_json() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        // Same slug in two workspaces, but one dir lacks queue.json (partial) →
        // not a real queue → no collision.
        std::fs::create_dir_all(paths.queue_dir("main", "invoices")).unwrap();
        std::fs::write(paths.queue_dir("main", "invoices").join("queue.json"), b"{}").unwrap();
        std::fs::create_dir_all(paths.queue_dir("phase-1", "invoices")).unwrap(); // no queue.json
        assert!(detect_slug_collisions(&paths).is_empty());
    }

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

    /// `trigger_condition` lives in `<slug>.py`, never in the rule JSON
    /// (see `snapshot::codec::rules`), so a JSON-only check can never see
    /// it. The server enforces 4000 characters and rejects anything longer
    /// with a permanent 400.
    #[test]
    fn field_limit_violations_reports_oversized_rule_trigger_condition() {
        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("my-rule.json"),
            br#"{"name":"My Rule","queues":[]}"#,
        )
        .unwrap();
        std::fs::write(rules_dir.join("my-rule.py"), "x".repeat(4001).as_bytes()).unwrap();

        let mut cl = ChangeList::default();
        cl.rules
            .insert("my-rule".to_string(), rules_dir.join("my-rule.json"));

        let v = cl.field_limit_violations();
        assert_eq!(v.len(), 1, "expected exactly one violation, got {v:?}");
        assert_eq!(v[0].kind, "rules");
        assert_eq!(v[0].field, "trigger_condition");
        assert_eq!(v[0].limit, 4000);
        assert_eq!(v[0].actual, 4001);
        assert_eq!(
            v[0].path,
            rules_dir.join("my-rule.py"),
            "the violation must point at the .py sidecar the user edits, not the JSON"
        );
    }

    /// Boundary: exactly at the limit passes, and so does the limit plus a
    /// trailing newline — rdc writes sidecars without one but editors add
    /// it, and the server trims before validating.
    #[test]
    fn rule_trigger_condition_at_limit_and_with_trailing_newline_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("my-rule.json"),
            br#"{"name":"My Rule","queues":[]}"#,
        )
        .unwrap();
        std::fs::write(
            rules_dir.join("my-rule.py"),
            format!("{}\n", "x".repeat(4000)).as_bytes(),
        )
        .unwrap();

        let mut cl = ChangeList::default();
        cl.rules
            .insert("my-rule".to_string(), rules_dir.join("my-rule.json"));

        assert_eq!(cl.field_limit_violations().len(), 0);
    }

    /// A rule with no `trigger_condition` has no sidecar at all; the check
    /// must not treat a missing file as an error.
    #[test]
    fn rule_without_trigger_condition_sidecar_is_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("my-rule.json"),
            br#"{"name":"My Rule","queues":[]}"#,
        )
        .unwrap();

        let mut cl = ChangeList::default();
        cl.rules
            .insert("my-rule".to_string(), rules_dir.join("my-rule.json"));

        assert_eq!(cl.field_limit_violations().len(), 0);
    }

    /// Schema formulas live in `formulas/<id>.py` and are spliced back into
    /// the schema body on push. The server caps them at 2000 characters and
    /// answers an over-length one with a POSITIONAL error carrying no
    /// datapoint id at all, so naming the file is the whole point.
    #[test]
    fn field_limit_violations_reports_oversized_schema_formula() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path().join("workspaces/main/queues/invoices");
        std::fs::create_dir_all(queue_dir.join("formulas")).unwrap();
        std::fs::write(
            queue_dir.join("schema.json"),
            br#"{"name":"Invoices","content":[]}"#,
        )
        .unwrap();
        std::fs::write(
            queue_dir.join("formulas/total_amount.py"),
            "x".repeat(2001).as_bytes(),
        )
        .unwrap();
        std::fs::write(
            queue_dir.join("formulas/vendor_name.py"),
            "y".repeat(2000).as_bytes(),
        )
        .unwrap();

        let mut cl = ChangeList::default();
        cl.schemas
            .insert("invoices".to_string(), queue_dir.join("schema.json"));

        let v = cl.field_limit_violations();
        assert_eq!(v.len(), 1, "only the over-length formula should flag: {v:?}");
        assert_eq!(v[0].kind, "schemas");
        assert_eq!(v[0].field, "formula on datapoint 'total_amount'");
        assert_eq!(v[0].limit, 2000);
        assert_eq!(v[0].actual, 2001);
        assert_eq!(v[0].path, queue_dir.join("formulas/total_amount.py"));
    }

    /// A queue with no `formulas/` directory must not error.
    #[test]
    fn schema_without_formulas_dir_is_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path().join("workspaces/main/queues/invoices");
        std::fs::create_dir_all(&queue_dir).unwrap();
        std::fs::write(
            queue_dir.join("schema.json"),
            br#"{"name":"Invoices","content":[]}"#,
        )
        .unwrap();

        let mut cl = ChangeList::default();
        cl.schemas
            .insert("invoices".to_string(), queue_dir.join("schema.json"));

        assert_eq!(cl.field_limit_violations().len(), 0);
    }
}
