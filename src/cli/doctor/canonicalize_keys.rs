//! `rdc doctor` sub-step: rewrite on-disk JSON files in canonical key
//! order — the same shape a fresh `rdc sync` would produce — for any
//! file that drifted (hand edit, legacy pull from before
//! `HOOK_KEY_ORDER` existed, etc.).
//!
//! Strictly cosmetic: `content_hash` canonicalises through
//! `snapshot::noise::canonicalize_for_hash`, which sorts keys
//! alphabetically before hashing, so reordering the on-disk bytes does
//! NOT shift any lockfile hash. The pass is therefore safe to run
//! automatically (no per-file confirm) and never invalidates the
//! lockfile's merge base.
//!
//! Per-kind routing mirrors each snapshot writer's existing path
//! layout. Each kind's canonical bytes come from the same serializer
//! the pull driver uses — so doctor's output is byte-identical to
//! what `rdc sync` would have written.
//!
//! `--check` prints what would change without writing.
//!
//! Files that fail to parse (corrupted JSON, model-incompatible
//! hand-edits) are logged and skipped — doctor never aborts on a
//! single bad file.
//!
//! Files where the canonical bytes already match the on-disk bytes are
//! silent no-ops: this is the steady-state outcome on a clean env.

use crate::log::{Action, Log};
use crate::paths::Paths;
use crate::snapshot::writer::write_atomic;
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;

/// Per-kind file traversal + canonical-roundtrip. Returns the number
/// of files actually changed (under `--check` this is the count that
/// WOULD change). Errors only on the rare unrecoverable IO problem;
/// per-file parse failures are logged and skipped so one bad file
/// can't block the rest.
pub fn run(paths: &Paths, check: bool, log: &Arc<Log>) -> Result<usize> {
    let mut total = 0usize;

    // Organization (singleton).
    total += canonicalize_organization(paths, check, log)?;

    // Hooks — the kind with priority-key reorder (HOOK_KEY_ORDER).
    // This is the historical case the feature was designed for:
    // hooks written by older rdc versions where the on-disk key order
    // matched the API's arbitrary response order.
    total += canonicalize_hooks(paths, check, log)?;

    // Rules + schemas have sidecar code/formula files alongside JSON;
    // the canonicalize step touches only the JSON.
    total += canonicalize_rules(paths, check, log)?;

    // Flat single-file kinds via the generic typed-roundtrip helper.
    total += canonicalize_labels(paths, check, log)?;
    total += canonicalize_engines(paths, check, log)?;
    total += canonicalize_engine_fields(paths, check, log)?;
    total += canonicalize_workflows(paths, check, log)?;
    total += canonicalize_workflow_steps(paths, check, log)?;

    // Workspace tree: workspace.json + per-queue (queue.json,
    // schema.json, inbox.json, email-templates/*).
    total += canonicalize_workspace_tree(paths, check, log)?;

    if total == 0 {
        log.event(
            Action::Info,
            "all on-disk JSON already in canonical key order",
        );
    } else if check {
        log.event(
            Action::Info,
            &format!("{total} file(s) would be canonicalized (run without --check to apply)"),
        );
    } else {
        log.event(
            Action::Done,
            &format!("canonicalized {total} file(s)"),
        );
    }
    Ok(total)
}

fn canonicalize_organization(paths: &Paths, check: bool, log: &Arc<Log>) -> Result<usize> {
    let path = paths.organization_file();
    if !path.exists() {
        return Ok(0);
    }
    let current = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return parse_warn(log, &path, &e.to_string()),
    };
    let typed: crate::model::Organization = match serde_json::from_slice(&current) {
        Ok(v) => v,
        Err(e) => return parse_warn(log, &path, &e.to_string()),
    };
    let canonical = crate::snapshot::key_order::serialize_for_disk(&typed)?;
    apply_change(&path, &current, &canonical, check, log)
}

fn canonicalize_hooks(paths: &Paths, check: bool, log: &Arc<Log>) -> Result<usize> {
    let mut count = 0;
    for entry in iter_json_files(&paths.hooks_dir(), paths.env()) {
        let path = entry;
        let current = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                count += parse_warn(log, &path, &e.to_string())?;
                continue;
            }
        };
        // Re-parse to typed and re-serialize via the same per-kind
        // serializer the pull driver uses (`serialize_hook` applies
        // HOOK_KEY_ORDER + sorts queues + strips hidden fields).
        let typed: crate::model::Hook = match serde_json::from_slice(&current) {
            Ok(v) => v,
            Err(e) => {
                count += parse_warn(log, &path, &e.to_string())?;
                continue;
            }
        };
        let (canonical, _code) = crate::snapshot::hook::serialize_hook(&typed)?;
        count += apply_change(&path, &current, &canonical, check, log)?;
    }
    Ok(count)
}

fn canonicalize_rules(paths: &Paths, check: bool, log: &Arc<Log>) -> Result<usize> {
    let mut count = 0;
    for path in iter_json_files(&paths.rules_dir(), paths.env()) {
        let current = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                count += parse_warn(log, &path, &e.to_string())?;
                continue;
            }
        };
        let typed: crate::model::Rule = match serde_json::from_slice(&current) {
            Ok(v) => v,
            Err(e) => {
                count += parse_warn(log, &path, &e.to_string())?;
                continue;
            }
        };
        let (canonical, _code) = crate::snapshot::rule::serialize_rule(&typed)?;
        count += apply_change(&path, &current, &canonical, check, log)?;
    }
    Ok(count)
}

fn canonicalize_labels(paths: &Paths, check: bool, log: &Arc<Log>) -> Result<usize> {
    typed_roundtrip_dir::<crate::model::Label>(&paths.labels_dir(), paths.env(), check, log)
}

fn canonicalize_engines(paths: &Paths, check: bool, log: &Arc<Log>) -> Result<usize> {
    let mut count = 0;
    let engines_dir = paths.engines_dir();
    if !engines_dir.exists() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(&engines_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join("engine.json");
        if !path.exists() {
            continue;
        }
        count += roundtrip_one::<crate::model::Engine>(&path, check, log)?;
    }
    Ok(count)
}

fn canonicalize_engine_fields(paths: &Paths, check: bool, log: &Arc<Log>) -> Result<usize> {
    let mut count = 0;
    let engines_dir = paths.engines_dir();
    if !engines_dir.exists() {
        return Ok(0);
    }
    for engine_entry in std::fs::read_dir(&engines_dir)? {
        let engine_entry = engine_entry?;
        if !engine_entry.file_type()?.is_dir() {
            continue;
        }
        let fields_dir = engine_entry.path().join("fields");
        if !fields_dir.exists() {
            continue;
        }
        for path in iter_json_files(&fields_dir, paths.env()) {
            count += roundtrip_one::<crate::model::EngineField>(&path, check, log)?;
        }
    }
    Ok(count)
}

fn canonicalize_workflows(paths: &Paths, check: bool, log: &Arc<Log>) -> Result<usize> {
    let mut count = 0;
    let workflows_dir = paths.workflows_dir();
    if !workflows_dir.exists() {
        return Ok(0);
    }
    for entry in std::fs::read_dir(&workflows_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join("workflow.json");
        if !path.exists() {
            continue;
        }
        count += roundtrip_one::<crate::model::Workflow>(&path, check, log)?;
    }
    Ok(count)
}

fn canonicalize_workflow_steps(paths: &Paths, check: bool, log: &Arc<Log>) -> Result<usize> {
    let mut count = 0;
    let workflows_dir = paths.workflows_dir();
    if !workflows_dir.exists() {
        return Ok(0);
    }
    for w_entry in std::fs::read_dir(&workflows_dir)? {
        let w_entry = w_entry?;
        if !w_entry.file_type()?.is_dir() {
            continue;
        }
        let steps_dir = w_entry.path().join("steps");
        if !steps_dir.exists() {
            continue;
        }
        for path in iter_json_files(&steps_dir, paths.env()) {
            count += roundtrip_one::<crate::model::WorkflowStep>(&path, check, log)?;
        }
    }
    Ok(count)
}

fn canonicalize_workspace_tree(paths: &Paths, check: bool, log: &Arc<Log>) -> Result<usize> {
    let mut count = 0;
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return Ok(0);
    }
    for ws_entry in std::fs::read_dir(&workspaces_dir)? {
        let ws_entry = ws_entry?;
        if !ws_entry.file_type()?.is_dir() {
            continue;
        }
        let ws_dir = ws_entry.path();
        let ws_json = ws_dir.join("workspace.json");
        if ws_json.exists() {
            count += roundtrip_one::<crate::model::Workspace>(&ws_json, check, log)?;
        }
        let queues_dir = ws_dir.join("queues");
        if !queues_dir.exists() {
            continue;
        }
        for q_entry in std::fs::read_dir(&queues_dir)? {
            let q_entry = q_entry?;
            if !q_entry.file_type()?.is_dir() {
                continue;
            }
            let q_dir = q_entry.path();
            let q_json = q_dir.join("queue.json");
            if q_json.exists() {
                count += roundtrip_one::<crate::model::Queue>(&q_json, check, log)?;
            }
            let s_json = q_dir.join("schema.json");
            if s_json.exists() {
                count += roundtrip_one::<crate::model::Schema>(&s_json, check, log)?;
            }
            let i_json = q_dir.join("inbox.json");
            if i_json.exists() {
                count += roundtrip_one::<crate::model::Inbox>(&i_json, check, log)?;
            }
            let tpl_dir = q_dir.join("email-templates");
            if tpl_dir.exists() {
                for path in iter_json_files(&tpl_dir, paths.env()) {
                    count += roundtrip_one::<crate::model::EmailTemplate>(&path, check, log)?;
                }
            }
        }
    }
    Ok(count)
}

/// Generic per-file roundtrip via `key_order::serialize_for_disk`: the
/// kind's typed model captures the canonical field order; serde +
/// `preserve_order` puts named fields first in declaration order, then
/// the flatten-extras IndexMap. This is what the pull driver writes
/// for every non-hook/rule/schema kind.
fn roundtrip_one<T>(path: &Path, check: bool, log: &Arc<Log>) -> Result<usize>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let current = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return parse_warn(log, path, &e.to_string()),
    };
    let typed: T = match serde_json::from_slice(&current) {
        Ok(v) => v,
        Err(e) => return parse_warn(log, path, &e.to_string()),
    };
    let canonical = crate::snapshot::key_order::serialize_for_disk(&typed)?;
    apply_change(path, &current, &canonical, check, log)
}

/// Helper: typed roundtrip every `*.json` directly under `dir`.
fn typed_roundtrip_dir<T>(dir: &Path, env: &str, check: bool, log: &Arc<Log>) -> Result<usize>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let mut count = 0;
    for path in iter_json_files(dir, env) {
        count += roundtrip_one::<T>(&path, check, log)?;
    }
    Ok(count)
}

/// Iterate every `*.json` directly inside `dir` (no recursion).
/// Returns an empty iterator when the dir doesn't exist. Skips
/// shadow-conflict artifacts (`*.<env>.json`).
fn iter_json_files(dir: &Path, env: &str) -> impl Iterator<Item = std::path::PathBuf> {
    let env = env.to_string();
    let entries: Vec<_> = std::fs::read_dir(dir)
        .ok()
        .map(|rd| rd.flatten().collect())
        .unwrap_or_default();
    entries.into_iter().filter_map(move |entry| {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if crate::paths::is_shadow_artifact(&name, &env) {
            return None;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            return None;
        }
        if !entry.file_type().ok()?.is_file() {
            return None;
        }
        Some(path)
    })
}

/// Common write-or-log step. Returns 1 if the file would change (or
/// did change), 0 otherwise.
fn apply_change(
    path: &Path,
    current: &[u8],
    canonical: &[u8],
    check: bool,
    log: &Arc<Log>,
) -> Result<usize> {
    if current == canonical {
        return Ok(0);
    }
    if check {
        log.event(
            Action::Doctor,
            &format!("would canonicalize {}", path.display()),
        );
    } else {
        write_atomic(path, canonical)?;
        log.event(Action::Doctor, &format!("canonicalized {}", path.display()));
    }
    Ok(1)
}

/// Log a per-file warning and continue with the next file. Doctor
/// never aborts on a single bad file — one corrupt JSON shouldn't
/// block the rest of the env.
fn parse_warn(log: &Arc<Log>, path: &Path, msg: &str) -> Result<usize> {
    log.event(
        Action::Warn,
        &format!("canonicalize: cannot read {} — {msg}; skipping", path.display()),
    );
    Ok(0)
}
