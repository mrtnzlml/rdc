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
    pub saved_views: BTreeMap<String, std::path::PathBuf>,
    pub queues: BTreeMap<String, std::path::PathBuf>,
    pub schemas: BTreeMap<String, std::path::PathBuf>,
    pub inboxes: BTreeMap<String, std::path::PathBuf>,
    pub email_templates: BTreeMap<String, std::path::PathBuf>,
    pub engines: BTreeMap<String, std::path::PathBuf>,
    pub engine_fields: BTreeMap<String, std::path::PathBuf>,
    /// The organization, when `organization.json` differs from its recorded
    /// base. A singleton (lockfile slug `"self"`), so an `Option` rather than a
    /// map — and there is no tombstone counterpart: rdc cannot delete an
    /// organization.
    pub organization: Option<std::path::PathBuf>,
}

impl ChangeList {
    pub fn total(&self) -> usize {
        self.workspaces.len()
            + self.hooks.len()
            + self.rules.len()
            + self.labels.len()
            + self.saved_views.len()
            + self.queues.len()
            + self.schemas.len()
            + self.inboxes.len()
            + self.email_templates.len()
            + self.engines.len()
            + self.engine_fields.len()
            + usize::from(self.organization.is_some())
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
        // The org singleton, wrapped so it can go through the same `check`.
        let org_map: BTreeMap<String, std::path::PathBuf> = self
            .organization
            .iter()
            .map(|p| ("self".to_string(), p.clone()))
            .collect();
        check("organization", &org_map);
        check("workspaces", &self.workspaces);
        check("hooks", &self.hooks);
        check("rules", &self.rules);
        check("labels", &self.labels);
        check("saved_views", &self.saved_views);
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

        // Populated for the `"schemas"` arm below with the ids
        // `merge_formulas` would actually splice a sidecar into — the
        // gate the formula-sidecar loop further down consults. Keyed by
        // slug so that loop (which walks `self.schemas` a second time,
        // for the `formulas/` directory rather than `schema.json` itself)
        // doesn't have to re-read and re-parse `schema.json`.
        let mut schema_formula_ids: BTreeMap<String, std::collections::BTreeSet<String>> =
            BTreeMap::new();

        let mut check = |kind: &'static str, map: &BTreeMap<String, std::path::PathBuf>| {
            for (slug, path) in map {
                let Ok(bytes) = std::fs::read(path) else {
                    continue; // unreadable — push surfaces I/O errors
                };
                let Ok(mut body) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                    continue; // unparseable — reported by json_parse_errors
                };
                crate::snapshot::create::strip_for_create(&mut body, kind);
                let nested = match kind {
                    "schemas" => {
                        schema_formula_ids.insert(
                            slug.clone(),
                            crate::snapshot::schema::formula_sidecar_ids(&body),
                        );
                        crate::snapshot::limits::check_schema_content(&body)
                    }
                    "rules" => crate::snapshot::limits::check_rule_actions(&body),
                    _ => Vec::new(),
                };
                for v in crate::snapshot::limits::check_field_limits(kind, &body)
                    .into_iter()
                    .chain(nested)
                {
                    out.push(violation(kind, slug, path.clone(), v));
                }
            }
        };
        check("workspaces", &self.workspaces);
        check("hooks", &self.hooks);
        check("rules", &self.rules);
        check("labels", &self.labels);
        check("saved_views", &self.saved_views);
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
                out.push(violation("rules", slug, py_path, v));
            }
        }

        // Schema formulas live in `<queue_dir>/formulas/<id>.py` and are
        // spliced back into the body on push — but only for the ids
        // `merge_formulas` would actually splice: a datapoint that both
        // still exists in `schema.json` AND carries no inline `formula`
        // key of its own. `read_local_formulas` returns every `.py` file
        // in the directory with no such filter, so an *orphaned* sidecar
        // (its datapoint renamed or deleted) or a *shadowed* one (its
        // datapoint already has an inline `formula`) would otherwise be
        // validated despite never reaching the wire — a false positive
        // that would wedge the project on a value the server never sees.
        // `ChangeList.schemas` stores the path to `schema.json`, so the
        // queue dir is its parent.
        for (slug, schema_path) in &self.schemas {
            let Some(queue_dir) = schema_path.parent() else {
                continue;
            };
            // No entry (schema.json unreadable/unparseable) means we don't
            // know which ids are live — skip rather than guess, the same
            // under-report-when-uncertain rule as everywhere else here.
            let Some(sidecar_ids) = schema_formula_ids.get(slug) else {
                continue;
            };
            let formulas =
                crate::snapshot::schema::read_local_formulas(queue_dir).unwrap_or_default();
            for (id, bytes) in formulas {
                if !sidecar_ids.contains(&id) {
                    continue; // orphaned or shadowed — merge_formulas would never splice this
                }
                let Ok(text) = String::from_utf8(bytes) else {
                    continue; // not UTF-8 — the push path surfaces that
                };
                if let Some(v) = crate::snapshot::limits::check_text(
                    format!("formula on datapoint '{id}'"),
                    crate::snapshot::limits::SCHEMA_FORMULA_LIMIT,
                    &text,
                ) {
                    out.push(violation(
                        "schemas",
                        slug,
                        queue_dir.join("formulas").join(format!("{id}.py")),
                        v,
                    ));
                }
            }
        }

        out
    }

    /// Validate every changed local file against the fields the API requires
    /// (see [`crate::snapshot::limits::required_for_create`]).
    ///
    /// Same permanence argument as [`ChangeList::field_limit_violations`]: the
    /// server's `400` can never be satisfied by retrying, and because the push
    /// phase precedes the pull phase its error aborts the whole cycle — leaving
    /// an env half-created, which is exactly how this surfaced (a first
    /// `migrate` into an empty env pushed its workspaces, schemas and queues,
    /// then died on the first `POST /inboxes`).
    ///
    /// Scoped to creates by the lockfile for most kinds: an object with an
    /// entry is PATCHed, and a PATCH that omits the key leaves the remote's
    /// value alone. That premise is false for a kind
    /// [`crate::snapshot::limits::required_for_create_also_applies_to_update`]
    /// names — there, the outgoing PATCH is the fully-typed model
    /// re-serialized, so an absent local field still reaches the wire as an
    /// explicit `null`/empty value, and an already-tracked object is checked
    /// too. Each file is stripped with `strip_for_create` first, so the check
    /// sees the bytes that actually reach the wire (an `email` key in the
    /// file, for instance, is gone by then and cannot mask a missing prefix).
    pub fn missing_create_fields(&self, lockfile: &Lockfile) -> Vec<MissingCreateField> {
        let mut out = Vec::new();
        let mut check = |kind: &'static str, map: &BTreeMap<String, std::path::PathBuf>| {
            if crate::snapshot::limits::required_for_create(kind).is_empty() {
                return;
            }
            let also_on_update =
                crate::snapshot::limits::required_for_create_also_applies_to_update(kind);
            for (slug, path) in map {
                let tracked = lockfile
                    .objects
                    .get(kind)
                    .and_then(|m| m.get(slug.as_str()))
                    .is_some();
                if tracked && !also_on_update {
                    continue; // a PATCH, not a POST
                }
                let Ok(bytes) = std::fs::read(path) else {
                    continue; // unreadable — push surfaces I/O errors
                };
                let Ok(mut body) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                    continue; // unparseable — reported by json_parse_errors
                };
                crate::snapshot::create::strip_for_create(&mut body, kind);
                for field in crate::snapshot::limits::missing_required_for_create(kind, &body) {
                    out.push(MissingCreateField {
                        kind,
                        slug: slug.clone(),
                        path: path.clone(),
                        field,
                    });
                }
            }
        };
        check("workspaces", &self.workspaces);
        check("hooks", &self.hooks);
        check("rules", &self.rules);
        check("labels", &self.labels);
        check("saved_views", &self.saved_views);
        check("queues", &self.queues);
        check("schemas", &self.schemas);
        check("inboxes", &self.inboxes);
        check("email_templates", &self.email_templates);
        check("engines", &self.engines);
        check("engine_fields", &self.engine_fields);
        out
    }

    /// Structural problems in the organization's `settings` — the subtree push
    /// sends. See [`crate::snapshot::limits::check_organization_settings`] for
    /// why this is worth catching offline.
    pub fn organization_settings_problems(
        &self,
    ) -> Vec<(std::path::PathBuf, crate::snapshot::limits::SettingsProblem)> {
        let Some(path) = &self.organization else {
            return Vec::new();
        };
        let Ok(bytes) = std::fs::read(path) else {
            return Vec::new();
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Vec::new(); // a parse error is reported by json_parse_errors
        };
        crate::snapshot::limits::check_organization_settings(&value)
            .into_iter()
            .map(|p| (path.clone(), p))
            .collect()
    }

    /// Local saved-view files rdc refuses to push because they are not shared.
    ///
    /// See `snapshot::limits::check_saved_view_shared` for why this is refused
    /// offline rather than pushed and reconciled.
    pub fn unshared_saved_views(&self) -> Vec<crate::snapshot::limits::UnsharedSavedView> {
        let mut out = Vec::new();
        for (slug, path) in &self.saved_views {
            let Ok(bytes) = std::fs::read(path) else {
                continue; // unreadable != unshared; push surfaces I/O errors
            };
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue; // unparseable is already reported by json_parse_errors
            };
            if !crate::snapshot::limits::check_saved_view_shared(&v) {
                out.push(crate::snapshot::limits::UnsharedSavedView {
                    slug: slug.clone(),
                    path: path.clone(),
                });
            }
        }
        out
    }

    /// Does this struct have a slot for `kind` at all?
    ///
    /// Distinguishes "a kind rdc does not push" from "a kind rdc pushes that
    /// happens to have no changes right now" — [`Self::contains`] answers
    /// `false` for both.
    pub fn tracks(&self, kind: &str) -> bool {
        matches!(
            kind,
            "workspaces"
                | "queues"
                | "schemas"
                | "inboxes"
                | "email_templates"
                | "hooks"
                | "rules"
                | "labels"
                | "saved_views"
                | "engines"
                | "engine_fields"
                | "organization"
        )
    }

    /// Is `(kind, slug)` in this change list?
    ///
    /// `organization` is a singleton keyed by the reserved slug `"self"` and
    /// stored as an `Option` rather than a map, so it answers through this same
    /// call rather than forcing every caller to special-case it.
    pub fn contains(&self, kind: &str, slug: &str) -> bool {
        match kind {
            "workspaces" => self.workspaces.contains_key(slug),
            "queues" => self.queues.contains_key(slug),
            "schemas" => self.schemas.contains_key(slug),
            "inboxes" => self.inboxes.contains_key(slug),
            "email_templates" => self.email_templates.contains_key(slug),
            "hooks" => self.hooks.contains_key(slug),
            "rules" => self.rules.contains_key(slug),
            "labels" => self.labels.contains_key(slug),
            "saved_views" => self.saved_views.contains_key(slug),
            "engines" => self.engines.contains_key(slug),
            "engine_fields" => self.engine_fields.contains_key(slug),
            "organization" => slug == "self" && self.organization.is_some(),
            _ => false,
        }
    }
}

/// Build a [`FieldLimitViolation`] from a [`crate::snapshot::limits::LimitViolation`],
/// filling in the object-identifying fields the limits module doesn't
/// know about (`kind`, `slug`, `path`). The one place this mapping is
/// written, so the several call sites in `field_limit_violations` — the
/// top-level + nested JSON walk, the rule `trigger_condition` sidecar, the
/// schema formula sidecar — cannot drift from each other.
fn violation(
    kind: &'static str,
    slug: &str,
    path: std::path::PathBuf,
    v: crate::snapshot::limits::LimitViolation,
) -> FieldLimitViolation {
    FieldLimitViolation {
        kind,
        slug: slug.to_string(),
        path,
        field: v.field,
        limit: v.limit,
        actual: v.actual,
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

/// One object the push would CREATE without a field the API requires on
/// create, as reported by [`ChangeList::missing_create_fields`].
#[derive(Debug)]
pub struct MissingCreateField {
    pub kind: &'static str,
    pub slug: String,
    pub path: std::path::PathBuf,
    /// The absent field, from [`crate::snapshot::limits::required_for_create`].
    pub field: &'static str,
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
    pub saved_views: BTreeMap<String, u64>,
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
            + self.saved_views.len()
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

    /// Does this struct have a slot for `kind` at all?
    ///
    /// `organization` is absent on purpose: rdc cannot delete an organization.
    pub fn tracks(&self, kind: &str) -> bool {
        matches!(
            kind,
            "workspaces"
                | "queues"
                | "schemas"
                | "inboxes"
                | "email_templates"
                | "hooks"
                | "rules"
                | "labels"
                | "saved_views"
                | "engines"
                | "engine_fields"
        )
    }

    /// Is `(kind, slug)` tombstoned?
    pub fn contains(&self, kind: &str, slug: &str) -> bool {
        match kind {
            "workspaces" => self.workspaces.contains_key(slug),
            "queues" => self.queues.contains_key(slug),
            "schemas" => self.schemas.contains_key(slug),
            "inboxes" => self.inboxes.contains_key(slug),
            "email_templates" => self.email_templates.contains_key(slug),
            "hooks" => self.hooks.contains_key(slug),
            "rules" => self.rules.contains_key(slug),
            "labels" => self.labels.contains_key(slug),
            "saved_views" => self.saved_views.contains_key(slug),
            "engines" => self.engines.contains_key(slug),
            "engine_fields" => self.engine_fields.contains_key(slug),
            _ => false,
        }
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
    scanned += scan_flat_kind(
        paths,
        lockfile,
        "saved_views",
        paths.saved_views_dir(),
        &mut changes.saved_views,
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
    scanned += scan_organization(paths, lockfile, &mut changes.organization)?;

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
    detect_flat(lockfile, "saved_views", &paths.saved_views_dir(), &mut t.saved_views);

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

/// Hash `organization.json` and report it when it differs from the lockfile
/// base. The org is a singleton (`slug = "self"`) with no create and no delete:
/// a MISSING file is not a tombstone — rdc cannot delete an organization — it
/// simply means there is nothing to push, and the pull half of the same sync
/// writes the file back.
fn scan_organization(
    paths: &Paths,
    lockfile: &Lockfile,
    out: &mut Option<std::path::PathBuf>,
) -> Result<usize> {
    use crate::state::content_hash;
    let path = paths.organization_file();
    if !path.exists() {
        return Ok(0);
    }
    let bytes = std::fs::read(&path)?;
    let local_hash = content_hash(&bytes, &crate::state::Lockfile::default());
    let base_hash = lockfile
        .objects
        .get("organization")
        .and_then(|m| m.get("self"))
        .and_then(|e| e.content_hash.as_deref());
    if base_hash != Some(local_hash.as_str()) {
        *out = Some(path);
    }
    Ok(1)
}

/// Convert a list of classified items (from `cli::sync::classify`) into a
/// push-side `ChangeList`. Only `LocalEdit` and `LocalCreate` items are
/// retained — those are the classes the push pipeline knows how to PATCH /
/// POST. `LocalDelete` is handled separately via tombstones; all remote-side
/// classes and `Clean` are silently dropped.
///
/// The classified items carry only `(kind, slug)`, so for each push-side
/// item this helper computes the on-disk path via the same layout the
/// `scan` walkers use. For flat kinds (`hooks`, `rules`, `labels`,
/// `saved_views`, etc.) the path is built directly from the slug; for
/// queue-nested kinds
/// (`queues`, `schemas`, `inboxes`) the lockfile keys items by queue slug
/// alone, so we sweep `workspaces/*/queues/<slug>/<file>` to find the
/// owning workspace. For `email_templates` the slug is already the
/// `<ws>/<queue>/<template>` compound, so the split is unambiguous. For
/// `engine_fields` we sweep `engines/*/fields/<slug>.json` (lockfile keys
/// fields by field slug alone, same as the existing scanner). `organization`
/// is a singleton keyed by the constant slug `"self"`, and its location is
/// fixed — no sweep needed. Kinds that don't go through the push pipeline at
/// all (`mdh`, `workflows`, `workflow_steps`) are silently dropped.
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
            "saved_views" => {
                cl.saved_views.insert(
                    it.slug.clone(),
                    paths.saved_views_dir().join(format!("{}.json", it.slug)),
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
            "organization" => {
                // Singleton: slug is always "self" and the on-disk location is
                // fixed, so — unlike queues/schemas/inboxes — no sweep is
                // needed to find it. `LocalCreate` is unreachable (the org
                // always exists remotely already) and `LocalDelete` has no
                // push meaning (see the doc comment above), so this only ever
                // fires for `LocalEdit`.
                cl.organization = Some(paths.organization_file());
            }
            // Other kinds (mdh, workflows, workflow_steps) don't go through
            // the push pipeline; silently drop — both are read-only at the
            // Rossum API.
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

    /// Helper: a ChangeList holding one inbox file with the given body.
    fn inbox_change(
        dir: &std::path::Path,
        body: serde_json::Value,
    ) -> (ChangeList, std::path::PathBuf) {
        let p = dir.join("inbox.json");
        std::fs::write(&p, serde_json::to_vec(&body).unwrap()).unwrap();
        let mut cl = ChangeList::default();
        cl.inboxes.insert("invoices".to_string(), p.clone());
        (cl, p)
    }

    /// The failure this check exists for: an inbox with no `email_prefix` that
    /// the push would POST. `POST /inboxes` rejects it permanently, aborting
    /// the cycle after earlier kinds are already created.
    #[test]
    fn missing_create_fields_reports_new_inbox_without_email_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let (cl, p) = inbox_change(tmp.path(), serde_json::json!({ "name": "In" }));

        let v = cl.missing_create_fields(&Lockfile::default());
        assert_eq!(v.len(), 1, "expected exactly one missing field: {v:?}");
        assert_eq!(v[0].kind, "inboxes");
        assert_eq!(v[0].slug, "invoices");
        assert_eq!(v[0].path, p);
        assert_eq!(v[0].field, "email_prefix");
    }

    #[test]
    fn missing_create_fields_ignores_new_inbox_with_email_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let (cl, _) = inbox_change(
            tmp.path(),
            serde_json::json!({ "name": "In", "email_prefix": "acme" }),
        );
        assert!(cl.missing_create_fields(&Lockfile::default()).is_empty());
    }

    /// A tracked inbox is PATCHed, and a PATCH that omits `email_prefix`
    /// leaves the remote's own address alone — blocking it would refuse a
    /// legitimate push.
    #[test]
    fn missing_create_fields_ignores_a_tracked_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let (cl, _) = inbox_change(tmp.path(), serde_json::json!({ "name": "In" }));
        let mut lf = Lockfile::default();
        lf.upsert(
            "inboxes",
            "invoices",
            crate::state::ObjectEntry {
                id: 7,
                modified_at: None,
                modified_by: None,
                content_hash: None,
                secrets_hash: None,
            },
        );
        assert!(cl.missing_create_fields(&lf).is_empty());
    }

    /// A hand-written `email` cannot stand in for the prefix: `strip_for_create`
    /// removes it before the POST, so the body still reaches the API without
    /// either field.
    #[test]
    fn missing_create_fields_does_not_accept_a_stripped_email() {
        let tmp = tempfile::tempdir().unwrap();
        let (cl, _) = inbox_change(
            tmp.path(),
            serde_json::json!({ "name": "In", "email": "in-a1b2c3@acme.rossum.app" }),
        );
        let v = cl.missing_create_fields(&Lockfile::default());
        assert_eq!(v.len(), 1, "strip_for_create drops `email`: {v:?}");
        assert_eq!(v[0].field, "email_prefix");
    }

    /// Helper: a ChangeList holding one saved-view file with the given body.
    fn saved_view_change(
        dir: &std::path::Path,
        body: serde_json::Value,
    ) -> (ChangeList, std::path::PathBuf) {
        let p = dir.join("mine.json");
        std::fs::write(&p, serde_json::to_vec(&body).unwrap()).unwrap();
        let mut cl = ChangeList::default();
        cl.saved_views.insert("mine".to_string(), p.clone());
        (cl, p)
    }

    /// The failure this check exists for: `POST /saved_views` requires
    /// `query`, and a hand-authored file that never sets it must be refused
    /// offline rather than sent.
    #[test]
    fn missing_create_fields_reports_new_saved_view_without_query() {
        let tmp = tempfile::tempdir().unwrap();
        let (cl, p) =
            saved_view_change(tmp.path(), serde_json::json!({ "name": "Mine", "shared": true }));

        let v = cl.missing_create_fields(&Lockfile::default());
        assert_eq!(v.len(), 1, "expected exactly one missing field: {v:?}");
        assert_eq!(v[0].kind, "saved_views");
        assert_eq!(v[0].slug, "mine");
        assert_eq!(v[0].path, p);
        assert_eq!(v[0].field, "query");
    }

    #[test]
    fn missing_create_fields_reports_new_saved_view_without_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (cl, _) = saved_view_change(
            tmp.path(),
            serde_json::json!({ "shared": true, "query": { "$and": [] } }),
        );
        let v = cl.missing_create_fields(&Lockfile::default());
        assert_eq!(v.len(), 1, "expected exactly one missing field: {v:?}");
        assert_eq!(v[0].field, "name");
    }

    #[test]
    fn missing_create_fields_ignores_new_saved_view_with_name_and_query() {
        let tmp = tempfile::tempdir().unwrap();
        let (cl, _) = saved_view_change(
            tmp.path(),
            serde_json::json!({ "name": "Mine", "shared": true, "query": { "$and": [] } }),
        );
        assert!(cl.missing_create_fields(&Lockfile::default()).is_empty());
    }

    /// The distinctive case this per-kind toggle exists for: unlike every
    /// other kind (see `missing_create_fields_ignores_a_tracked_inbox`), an
    /// already-TRACKED saved view is still checked. Its outgoing PATCH is the
    /// fully-typed `SavedView` model re-serialized, so a local file that never
    /// sets `query` still sends `"query": null` on update — omitting the key
    /// locally does not omit it on the wire.
    #[test]
    fn missing_create_fields_reports_a_tracked_saved_view_missing_query() {
        let tmp = tempfile::tempdir().unwrap();
        let (cl, _) =
            saved_view_change(tmp.path(), serde_json::json!({ "name": "Mine", "shared": true }));
        let mut lf = Lockfile::default();
        lf.upsert(
            "saved_views",
            "mine",
            crate::state::ObjectEntry {
                id: 9,
                modified_at: None,
                modified_by: None,
                content_hash: Some("h".into()),
                secrets_hash: None,
            },
        );
        let v = cl.missing_create_fields(&lf);
        assert_eq!(
            v.len(),
            1,
            "a tracked saved view missing `query` must still be refused: {v:?}"
        );
        assert_eq!(v[0].field, "query");
    }

    #[test]
    fn missing_create_fields_ignores_a_tracked_saved_view_with_query() {
        let tmp = tempfile::tempdir().unwrap();
        let (cl, _) = saved_view_change(
            tmp.path(),
            serde_json::json!({ "name": "Mine", "shared": true, "query": { "$and": [] } }),
        );
        let mut lf = Lockfile::default();
        lf.upsert(
            "saved_views",
            "mine",
            crate::state::ObjectEntry {
                id: 9,
                modified_at: None,
                modified_by: None,
                content_hash: Some("h".into()),
                secrets_hash: None,
            },
        );
        assert!(cl.missing_create_fields(&lf).is_empty());
    }

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

    /// The gap a later reviewer found: `saved_views` had no arm in this
    /// match, so a locally-edited or newly-created saved view was silently
    /// dropped from the push-side `ChangeList` -- no error, no PATCH, no
    /// POST, and none of `push::saved_views`' own tests could ever catch it
    /// since they all drive the driver directly with a hand-built
    /// `BTreeMap`, never through this classifier. Pinned here, at the level
    /// where the omission actually lived.
    #[test]
    fn change_list_from_classified_includes_saved_views_for_edit_and_create() {
        use crate::cli::sync::classify::{ClassifiedItem, SyncClass};
        use crate::paths::Paths;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");

        let items = vec![
            ClassifiedItem {
                kind: "saved_views".into(),
                slug: "awaiting-approval".into(),
                class: SyncClass::LocalEdit,
                local_hash: Some("h".into()),
                remote_hash: Some("h".into()),
                base_hash: Some("h".into()),
            },
            ClassifiedItem {
                kind: "saved_views".into(),
                slug: "team-dashboard".into(),
                class: SyncClass::LocalCreate,
                local_hash: Some("h".into()),
                remote_hash: None,
                base_hash: None,
            },
            ClassifiedItem {
                kind: "saved_views".into(),
                slug: "ignored".into(),
                // Not a push-side class -- must be ignored, same as every
                // other kind.
                class: SyncClass::RemoteEdit,
                local_hash: Some("h".into()),
                remote_hash: Some("h2".into()),
                base_hash: Some("h".into()),
            },
        ];

        let cl = change_list_from_classified(&paths, &items);
        assert_eq!(
            cl.saved_views.len(),
            2,
            "expected the LocalEdit and the LocalCreate, not the RemoteEdit: {:?}",
            cl.saved_views
        );
        assert_eq!(
            cl.saved_views.get("awaiting-approval"),
            Some(&paths.saved_views_dir().join("awaiting-approval.json")),
        );
        assert_eq!(
            cl.saved_views.get("team-dashboard"),
            Some(&paths.saved_views_dir().join("team-dashboard.json")),
        );
        assert!(
            !cl.saved_views.contains_key("ignored"),
            "a RemoteEdit item must never land in the push-side ChangeList"
        );
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
    /// the schema body on push -- but only for a datapoint that both still
    /// exists in `schema.json` and carries no inline `formula` of its own
    /// (see `snapshot::schema::merge_formulas`). This is the true happy
    /// path: both sidecar ids are live datapoints with no inline formula,
    /// so `merge_formulas` would actually splice them. The server caps
    /// them at 2000 characters and answers an over-length one with a
    /// POSITIONAL error carrying no datapoint id at all, so naming the
    /// file is the whole point.
    #[test]
    fn field_limit_violations_reports_oversized_schema_formula() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path().join("workspaces/main/queues/invoices");
        std::fs::create_dir_all(queue_dir.join("formulas")).unwrap();
        std::fs::write(
            queue_dir.join("schema.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "Invoices",
                "content": [{
                    "category": "section",
                    "id": "s",
                    "children": [
                        { "category": "datapoint", "id": "total_amount" },
                        { "category": "datapoint", "id": "vendor_name" }
                    ]
                }]
            }))
            .unwrap(),
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

    /// An ORPHANED sidecar: `formulas/<id>.py` whose id is absent from the
    /// content tree entirely (the datapoint was renamed or deleted
    /// locally). `merge_formulas` would never splice it -- the push never
    /// sends it -- so it must not block the push. Before this gate, a user
    /// who dropped an over-length field by deleting the datapoint (leaving
    /// the stale `.py` behind) would find the project permanently wedged
    /// on a value the server would have accepted, with an error naming a
    /// datapoint that no longer exists.
    #[test]
    fn field_limit_violations_ignores_orphaned_schema_formula_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path().join("workspaces/main/queues/invoices");
        std::fs::create_dir_all(queue_dir.join("formulas")).unwrap();
        std::fs::write(
            queue_dir.join("schema.json"),
            br#"{"name":"Invoices","content":[]}"#,
        )
        .unwrap();
        // No datapoint anywhere in content claims this id.
        std::fs::write(
            queue_dir.join("formulas/total_amount.py"),
            "x".repeat(2001).as_bytes(),
        )
        .unwrap();

        let mut cl = ChangeList::default();
        cl.schemas
            .insert("invoices".to_string(), queue_dir.join("schema.json"));

        assert_eq!(
            cl.field_limit_violations().len(),
            0,
            "an orphaned sidecar the push will never send must not be flagged"
        );
    }

    /// A SHADOWED sidecar: the datapoint exists, but it carries an inline
    /// `formula` key of its own, so `merge_formulas` splices nothing (it
    /// only fills in a MISSING `formula`). The stale `.py` file must not
    /// be flagged. The inline value here is itself over-length, so it
    /// must still be reported -- exactly once, by the nested schema walk
    /// -- not twice.
    #[test]
    fn field_limit_violations_ignores_shadowed_schema_formula_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path().join("workspaces/main/queues/invoices");
        std::fs::create_dir_all(queue_dir.join("formulas")).unwrap();
        std::fs::write(
            queue_dir.join("schema.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "Invoices",
                "content": [{
                    "category": "datapoint",
                    "id": "total_amount",
                    "formula": "f".repeat(2001)
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        // Stale sidecar left over from before the datapoint grew an
        // inline formula. merge_formulas ignores it (the key is present).
        std::fs::write(
            queue_dir.join("formulas/total_amount.py"),
            "x".repeat(2500).as_bytes(),
        )
        .unwrap();

        let mut cl = ChangeList::default();
        cl.schemas
            .insert("invoices".to_string(), queue_dir.join("schema.json"));

        let v = cl.field_limit_violations();
        assert_eq!(
            v.len(),
            1,
            "the inline formula must be reported once, by the nested walk -- \
             the shadowed sidecar must not ALSO report it: {v:?}"
        );
        assert_eq!(v[0].field, "formula on datapoint 'total_amount'");
        assert_eq!(v[0].actual, 2001);
        // Reported from the nested walk, so the path is schema.json, not
        // the stale .py sidecar.
        assert_eq!(v[0].path, queue_dir.join("schema.json"));
    }

    /// Unlike the sidecar-extracted formula above, `prompt` stays inline in
    /// `schema.json`'s content tree. This exercises the `"schemas"` match
    /// arm in `field_limit_violations` itself (`check_schema_content` is
    /// only wired in for that one kind) -- the `snapshot::limits` unit
    /// tests call `check_schema_content` directly and never touch that
    /// dispatch, so a typo in the match arm or a dropped `.chain(nested)`
    /// would pass every one of them while fully disabling this feature on
    /// the real push path.
    #[test]
    fn field_limit_violations_reports_oversized_nested_schema_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let queue_dir = dir.path().join("workspaces/main/queues/invoices");
        std::fs::create_dir_all(&queue_dir).unwrap();
        std::fs::write(
            queue_dir.join("schema.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "Invoices",
                "content": [{
                    "category": "section",
                    "id": "invoice_details",
                    "children": [{
                        "category": "datapoint",
                        "id": "invoice_id",
                        "prompt": "p".repeat(5001)
                    }]
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let mut cl = ChangeList::default();
        cl.schemas
            .insert("invoices".to_string(), queue_dir.join("schema.json"));

        let v = cl.field_limit_violations();
        assert_eq!(v.len(), 1, "only the over-length prompt should flag: {v:?}");
        assert_eq!(v[0].kind, "schemas");
        assert_eq!(v[0].field, "prompt on datapoint 'invoice_id'");
        assert_eq!(v[0].limit, 5000);
        assert_eq!(v[0].actual, 5001);
        // Nested values live directly in the JSON, so unlike the sidecar
        // cases above, the reported path is schema.json itself.
        assert_eq!(v[0].path, queue_dir.join("schema.json"));
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

    /// A rule's `actions[].payload.content` stays inline in the rule JSON
    /// (unlike `trigger_condition`, which lives in a `.py` sidecar). This
    /// exercises the `"rules"` match arm in `field_limit_violations` itself
    /// -- the `snapshot::limits` unit tests call `check_rule_actions`
    /// directly and never touch that dispatch, so a typo in the match arm
    /// or a dropped `.chain(nested)` would pass every one of them while
    /// fully disabling this feature on the real push path.
    #[test]
    fn field_limit_violations_reports_oversized_rule_action_content() {
        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("my-rule.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": "Example Rule",
                "actions": [
                    {
                        "id": "b7d5856b-7990-4c8f-8048-ca3b8e68239a",
                        "enabled": true,
                        "type": "show_message",
                        "event": "validation",
                        "payload": { "type": "warning", "content": "ok", "schema_id": "total" }
                    },
                    {
                        "id": "cf3e8c84-552c-482c-b1cf-333ace397a8c",
                        "enabled": true,
                        "type": "add_automation_blocker",
                        "event": "validation",
                        "payload": { "content": "c".repeat(4097), "schema_id": "total" }
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let mut cl = ChangeList::default();
        cl.rules
            .insert("my-rule".to_string(), rules_dir.join("my-rule.json"));

        let v = cl.field_limit_violations();
        assert_eq!(v.len(), 1, "only the over-length action payload should flag: {v:?}");
        assert_eq!(v[0].kind, "rules");
        assert_eq!(v[0].field, "actions[1] (add_automation_blocker) payload.content");
        assert_eq!(v[0].limit, 4096);
        assert_eq!(v[0].actual, 4097);
        // Nested values live directly in the JSON, so the reported path is
        // the rule's .json file itself, not a sidecar.
        assert_eq!(v[0].path, rules_dir.join("my-rule.json"));
    }

    #[test]
    fn scan_finds_a_new_saved_view_as_a_change() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.saved_views_dir()).unwrap();
        std::fs::write(
            paths.saved_views_dir().join("awaiting-approval.json"),
            br#"{"name":"Awaiting approval","shared":true,"query":{"$and":[]}}"#,
        )
        .unwrap();

        let lockfile = Lockfile::default();
        let (_n, changes, tombstones) = scan(&paths, &lockfile).unwrap();

        assert!(changes.saved_views.contains_key("awaiting-approval"));
        assert!(tombstones.saved_views.is_empty());
    }

    #[test]
    fn a_missing_saved_view_file_is_a_tombstone() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        let mut lockfile = Lockfile::default();
        lockfile.upsert(
            "saved_views",
            "gone",
            crate::state::ObjectEntry {
                id: 77,
                modified_at: None,
                modified_by: None,
                content_hash: Some("h".into()),
                secrets_hash: None,
            },
        );

        let t = detect_tombstones(&paths, &lockfile);
        assert_eq!(t.saved_views.get("gone"), Some(&77));
    }

    #[test]
    fn unshared_saved_view_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.saved_views_dir()).unwrap();
        let path = paths.saved_views_dir().join("mine.json");
        std::fs::write(&path, br#"{"name":"Mine","shared":false,"query":{"$and":[]}}"#).unwrap();

        let mut changes = ChangeList::default();
        changes.saved_views.insert("mine".to_string(), path.clone());

        let refused = changes.unshared_saved_views();
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].slug, "mine");
    }

    #[test]
    fn shared_saved_view_is_not_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "test");
        std::fs::create_dir_all(paths.saved_views_dir()).unwrap();
        let path = paths.saved_views_dir().join("ok.json");
        std::fs::write(&path, br#"{"name":"Ok","shared":true,"query":{"$and":[]}}"#).unwrap();

        let mut changes = ChangeList::default();
        changes.saved_views.insert("ok".to_string(), path);
        assert!(changes.unshared_saved_views().is_empty());
    }

    /// Per push-capable kind: the slug to classify, and the files that must
    /// exist on disk for the change-list arm to find it.
    ///
    /// Several arms only insert when a real file is found — `queues`,
    /// `schemas`, `inboxes` sweep `workspaces/*/queues/<slug>/`, and
    /// `engine_fields` resolves a `<engine>/<field>` composite key — so a
    /// synthetic item alone would silently not be inserted and the test would
    /// pass for the wrong reason.
    fn push_capable_fixture() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
        vec![
            ("workspaces", "main", vec!["workspaces/main/workspace.json"]),
            ("queues", "invoices", vec!["workspaces/main/queues/invoices/queue.json"]),
            ("schemas", "invoices", vec!["workspaces/main/queues/invoices/schema.json"]),
            ("inboxes", "invoices", vec!["workspaces/main/queues/invoices/inbox.json"]),
            (
                "email_templates",
                "main/invoices/ack",
                vec!["workspaces/main/queues/invoices/email-templates/ack.json"],
            ),
            ("hooks", "validator", vec!["hooks/validator.json"]),
            ("rules", "totals", vec!["rules/totals.json"]),
            ("labels", "urgent", vec!["labels/urgent.json"]),
            ("saved_views", "awaiting", vec!["saved-views/awaiting.json"]),
            ("engines", "extractor", vec!["engines/extractor/engine.json"]),
            ("engine_fields", "extractor/amount", vec!["engines/extractor/fields/amount.json"]),
            ("organization", "self", vec!["organization.json"]),
        ]
    }

    /// The fixture must cover PUSH_CAPABLE exactly. Without this, adding a kind
    /// to PUSH_CAPABLE and forgetting the fixture would make the enforcement
    /// test below quietly skip it — the same silent-omission failure this whole
    /// change exists to prevent.
    #[test]
    fn push_capable_fixture_covers_every_push_capable_kind() {
        let mut fixture: Vec<&str> = push_capable_fixture().iter().map(|(k, _, _)| *k).collect();
        let mut expected: Vec<&str> = crate::kinds::PUSH_CAPABLE.to_vec();
        fixture.sort_unstable();
        expected.sort_unstable();
        assert_eq!(fixture, expected);
    }

    #[test]
    fn every_push_capable_kind_has_a_change_list_slot() {
        let cl = ChangeList::default();
        for kind in crate::kinds::PUSH_CAPABLE {
            assert!(cl.tracks(kind), "ChangeList has no slot for '{kind}'");
        }
    }

    #[test]
    fn every_deletable_kind_has_a_tombstones_slot() {
        let t = Tombstones::default();
        for kind in crate::kinds::DELETABLE {
            assert!(t.tracks(kind), "Tombstones has no slot for '{kind}'");
        }
        assert!(
            !t.tracks("organization"),
            "an organization can never be deleted, so it must have no tombstone slot",
        );
    }

    /// The regression guard for the worst bug on the saved-views branch:
    /// `change_list_from_classified` silently dropped a kind with no arm, so
    /// every ordinary local edit of it made no request at all.
    #[test]
    fn every_push_capable_kind_reaches_the_change_list() {
        use crate::cli::sync::classify::{ClassifiedItem, SyncClass};

        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        let root = paths.env_root();

        let fixture = push_capable_fixture();
        for (_, _, files) in &fixture {
            for rel in files {
                let p = root.join(rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, b"{}").unwrap();
            }
        }

        let items: Vec<ClassifiedItem> = fixture
            .iter()
            .map(|(kind, slug, _)| ClassifiedItem {
                kind: (*kind).to_string(),
                slug: (*slug).to_string(),
                class: SyncClass::LocalEdit,
                local_hash: None,
                remote_hash: None,
                base_hash: None,
            })
            .collect();

        let cl = change_list_from_classified(&paths, &items);

        for (kind, slug, _) in &fixture {
            assert!(
                cl.contains(kind, slug),
                "'{kind}' is push-capable but change_list_from_classified dropped it",
            );
        }

        // `total()` is its own hand-written sum, and `is_empty()` is defined on
        // it: a kind that reaches the change list but is missing from `total()`
        // makes an otherwise-nonempty list look empty and skips the WHOLE push
        // phase. `contains` above reads the field directly and would not notice.
        // One item per push-capable kind went in, so the sum must be exactly
        // that many.
        assert_eq!(
            cl.total(),
            crate::kinds::PUSH_CAPABLE.len(),
            "ChangeList::total() does not count every push-capable kind",
        );
    }

    /// A class that is not a local change must never reach the change list.
    #[test]
    fn a_non_local_class_does_not_reach_the_change_list() {
        use crate::cli::sync::classify::{ClassifiedItem, SyncClass};
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");
        std::fs::create_dir_all(paths.labels_dir()).unwrap();
        std::fs::write(paths.labels_dir().join("urgent.json"), b"{}").unwrap();

        let items = vec![ClassifiedItem {
            kind: "labels".to_string(),
            slug: "urgent".to_string(),
            class: SyncClass::RemoteEdit,
            local_hash: None,
            remote_hash: None,
            base_hash: None,
        }];
        let cl = change_list_from_classified(&paths, &items);
        assert!(!cl.contains("labels", "urgent"));
    }

    /// The scan-side counterpart: `detect_tombstones` dispatches per kind too,
    /// and a deletable kind missing from it would mean a deleted local file
    /// never becomes a remote delete — the object would linger in the env
    /// forever with no diagnostic.
    ///
    /// Seeded with lockfile entries and an EMPTY tree, so every entry is
    /// file-less and must therefore be tombstoned.
    #[test]
    fn every_deletable_kind_reaches_the_tombstones() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_env(tmp.path(), "dev");

        // Compound-key kinds need a well-formed key: `detect_tombstones` splits
        // email_templates on '/' expecting `<ws>/<queue>/<template>`, and
        // engine_fields expecting `<engine>/<field>`.
        let slug_for = |kind: &str| -> &'static str {
            match kind {
                "email_templates" => "main/invoices/ack",
                "engine_fields" => "extractor/amount",
                _ => "thing",
            }
        };

        let mut lockfile = Lockfile::default();
        for kind in crate::kinds::DELETABLE {
            lockfile.upsert(
                kind,
                slug_for(kind),
                crate::state::ObjectEntry {
                    id: 1,
                    modified_at: None,
                    modified_by: None,
                    content_hash: Some("h".to_string()),
                    secrets_hash: None,
                },
            );
        }

        let t = detect_tombstones(&paths, &lockfile);
        for kind in crate::kinds::DELETABLE {
            assert!(
                t.contains(kind, slug_for(kind)),
                "'{kind}' is deletable but detect_tombstones did not tombstone it",
            );
        }
    }

    #[test]
    fn contains_is_false_for_an_untracked_kind() {
        let cl = ChangeList::default();
        assert!(!cl.tracks("mdh"));
        assert!(!cl.contains("mdh", "anything"));
        assert!(!cl.tracks("workflows"));
    }
}
