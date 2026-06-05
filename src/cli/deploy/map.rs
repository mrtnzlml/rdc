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
    added += match_kind(
        &mut mapping.hooks,
        &src_paths.hooks_dir(), src_paths.env(),
        &tgt_paths.hooks_dir(), tgt_paths.env(),
    )?;
    added += match_kind(
        &mut mapping.rules,
        &src_paths.rules_dir(), src_paths.env(),
        &tgt_paths.rules_dir(), tgt_paths.env(),
    )?;
    added += match_kind(
        &mut mapping.labels,
        &src_paths.labels_dir(), src_paths.env(),
        &tgt_paths.labels_dir(), tgt_paths.env(),
    )?;
    added += match_queues(&mut mapping.queues, src_paths, tgt_paths)?;
    added += match_schemas(&mut mapping.schemas, src_paths, tgt_paths)?;
    added += match_inboxes(&mut mapping.inboxes, src_paths, tgt_paths)?;
    added += match_email_templates(&mut mapping.email_templates, src_paths, tgt_paths)?;
    added += match_engines(&mut mapping.engines, src_paths, tgt_paths)?;
    added += match_engine_fields(&mut mapping.engine_fields, src_paths, tgt_paths)?;
    Ok(added)
}

/// Fail if any source slug referenced by the mapping is absent from the src
/// snapshot. Auto-matched entries always exist (they're listed off disk), so
/// in practice this only catches hand-curated mapping-file entries — a typo or
/// a stale cross-env rename whose source object was renamed/removed. The deploy
/// used to warn and silently skip such entries deep in the apply loops (so the
/// object the user meant to deploy never got deployed); this stops the deploy
/// up front, before any remote writes, with the full list of offenders.
///
/// `hook_templates` is intentionally excluded — it pairs cross-cluster URLs,
/// not on-disk source objects.
pub fn validate_mapping_sources(
    mapping: &Mapping,
    src_paths: &Paths,
    mapping_file: &Path,
) -> Result<()> {
    use std::collections::HashSet;
    let env = src_paths.env();

    // Existing source slugs per kind, computed with the SAME enumerators
    // `auto_match` uses, so existence is judged identically.
    let workspaces: HashSet<String> = list_workspace_slugs(src_paths)?.into_iter().collect();
    let hooks: HashSet<String> = list_flat_slugs(&src_paths.hooks_dir(), env)?.into_iter().collect();
    let rules: HashSet<String> = list_flat_slugs(&src_paths.rules_dir(), env)?.into_iter().collect();
    let labels: HashSet<String> =
        list_flat_slugs(&src_paths.labels_dir(), env)?.into_iter().collect();
    let queues: HashSet<String> = collect_queue_slugs(src_paths)?.into_iter().collect();
    let schemas: HashSet<String> =
        collect_queue_slugs_with_file(src_paths, "schema.json")?.into_iter().collect();
    let inboxes: HashSet<String> =
        collect_queue_slugs_with_file(src_paths, "inbox.json")?.into_iter().collect();
    let email_templates: HashSet<String> =
        collect_email_template_keys(src_paths)?.into_iter().collect();
    let engines: HashSet<String> = list_engine_slugs(src_paths)?.into_iter().collect();
    let engine_fields: HashSet<String> = list_engine_field_slugs(src_paths)?.into_iter().collect();

    // `hook_templates` is excluded on purpose: it maps cross-cluster URLs,
    // not on-disk source objects.
    let kinds: [(&str, &BTreeMap<String, String>, &HashSet<String>); 10] = [
        ("workspaces", &mapping.workspaces, &workspaces),
        ("hooks", &mapping.hooks, &hooks),
        ("rules", &mapping.rules, &rules),
        ("labels", &mapping.labels, &labels),
        ("queues", &mapping.queues, &queues),
        ("schemas", &mapping.schemas, &schemas),
        ("inboxes", &mapping.inboxes, &inboxes),
        ("email_templates", &mapping.email_templates, &email_templates),
        ("engines", &mapping.engines, &engines),
        ("engine_fields", &mapping.engine_fields, &engine_fields),
    ];

    let mut missing: Vec<String> = Vec::new();
    for (kind, map, existing) in kinds {
        for (src_slug, tgt_slug) in map {
            if !existing.contains(src_slug) {
                missing.push(format!("  {kind}/{src_slug} -> {tgt_slug}"));
            }
        }
    }
    if !missing.is_empty() {
        missing.sort();
        anyhow::bail!(
            "deploy mapping references {} source object(s) that don't exist in '{}':\n{}\n\n\
             Fix the mapping file ({}): remove the stale entries or correct the source slugs, then re-run.",
            missing.len(),
            env,
            missing.join("\n"),
            mapping_file.display(),
        );
    }
    Ok(())
}

fn match_kind(
    existing: &mut BTreeMap<String, String>,
    src_dir: &Path,
    src_env: &str,
    tgt_dir: &Path,
    tgt_env: &str,
) -> Result<usize> {
    let src_slugs = list_flat_slugs(src_dir, src_env)?;
    let tgt_slugs: std::collections::HashSet<_> =
        list_flat_slugs(tgt_dir, tgt_env)?.into_iter().collect();
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

fn list_flat_slugs(dir: &Path, env: &str) -> Result<Vec<String>> {
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
        if crate::paths::is_shadow_artifact(&name, env) {
            continue;
        }
        if let Some(slug) = name.strip_suffix(".json") {
            out.push(slug.to_string());
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

/// List engine-field composite keys (`<engine_slug>/<field_slug>`) across
/// every engine. Engine fields are scoped per-engine, so two engines can
/// carry a field with the same field-slug — they only differ in the
/// `<engine_slug>/` prefix, which is what makes the key globally unique
/// for lockfile / mapping purposes.
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
            if crate::paths::is_shadow_artifact(&name, paths.env()) {
                continue;
            }
            if let Some(f_slug) = name.strip_suffix(".json") {
                out.push(format!("{e_slug}/{f_slug}"));
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
                if crate::paths::is_shadow_artifact(&name, paths.env()) {
                    continue;
                }
                if let Some(template_slug) = name.strip_suffix(".json") {
                    out.push(format!("{ws_slug}/{q_slug}/{template_slug}"));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping::Mapping;

    fn write_queue(paths: &Paths, ws_slug: &str, q_slug: &str) {
        let dir = paths.queue_dir(ws_slug, q_slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("queue.json"), b"{}").unwrap();
        std::fs::write(dir.join("schema.json"), b"{}").unwrap();
    }

    fn write_engine(paths: &Paths, slug: &str) {
        let dir = paths.engines_dir().join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("engine.json"), b"{}").unwrap();
    }

    #[test]
    fn validate_mapping_sources_errors_on_missing_source() {
        let src = tempfile::TempDir::new().unwrap();
        let src_paths = Paths::for_env(src.path(), "src");
        write_queue(&src_paths, "ws", "real-q"); // queue.json + schema.json
        write_engine(&src_paths, "real-engine");

        let mut mapping = Mapping::default();
        mapping.queues.insert("real-q".into(), "real-q".into()); // exists
        mapping.queues.insert("ghost-q".into(), "renamed-q".into()); // missing source
        mapping.engines.insert("ghost-engine".into(), "x".into()); // missing source

        let mf = src.path().join("map.toml");
        let err = validate_mapping_sources(&mapping, &src_paths, &mf).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ghost-q"), "must name the missing queue: {msg}");
        assert!(msg.contains("ghost-engine"), "must name the missing engine: {msg}");
        assert!(
            !msg.contains("real-q") && !msg.contains("real-engine"),
            "must not flag sources that DO exist: {msg}"
        );
    }

    #[test]
    fn validate_mapping_sources_ok_when_all_present() {
        let src = tempfile::TempDir::new().unwrap();
        let src_paths = Paths::for_env(src.path(), "src");
        write_queue(&src_paths, "ws", "real-q");

        let mut mapping = Mapping::default();
        mapping.queues.insert("real-q".into(), "renamed-on-tgt".into());
        mapping.schemas.insert("real-q".into(), "real-q".into());

        let mf = src.path().join("map.toml");
        assert!(validate_mapping_sources(&mapping, &src_paths, &mf).is_ok());
    }

    /// Two queues with the same NAME live in different workspaces. The sync
    /// fix assigns them globally-unique slugs (`shared-queue` /
    /// `shared-queue-2`) and distinct dirs. `auto_match` must then map BOTH
    /// distinctly across envs — never collapse them via the `dedup()` in
    /// `collect_queue_slugs` (the pre-fix cross-workspace deploy hazard).
    #[test]
    fn auto_match_keeps_same_named_queues_distinct_across_workspaces() {
        let src = tempfile::TempDir::new().unwrap();
        let tgt = tempfile::TempDir::new().unwrap();
        let src_paths = Paths::for_env(src.path(), "src");
        let tgt_paths = Paths::for_env(tgt.path(), "tgt");

        // Post-fix on-disk layout: same-named queues get distinct slugs in
        // their own workspaces.
        for paths in [&src_paths, &tgt_paths] {
            write_queue(paths, "workspace-alpha", "shared-queue");
            write_queue(paths, "workspace-beta", "shared-queue-2");
        }

        let mut mapping = Mapping::default();
        auto_match(&mut mapping, &src_paths, &tgt_paths).unwrap();

        // BOTH queues mapped, distinctly — no collapse.
        assert_eq!(mapping.queues.len(), 2, "both same-named queues must map: {:?}", mapping.queues);
        assert_eq!(mapping.queues.get("shared-queue").map(String::as_str), Some("shared-queue"));
        assert_eq!(
            mapping.queues.get("shared-queue-2").map(String::as_str),
            Some("shared-queue-2")
        );
        // Schemas (keyed by queue slug) likewise both mapped.
        assert_eq!(mapping.schemas.len(), 2, "both schemas must map: {:?}", mapping.schemas);
        assert!(mapping.schemas.contains_key("shared-queue"));
        assert!(mapping.schemas.contains_key("shared-queue-2"));
    }
}
