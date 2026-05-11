use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::paths::Paths;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

pub async fn run(src: &str, tgt: &str, check: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let src_paths = Paths::for_env(&cwd, src);
    let tgt_paths = Paths::for_env(&cwd, tgt);

    let cfg = ProjectConfig::load(&src_paths.project_config())
        .with_context(|| format!("loading project config from {}", src_paths.project_config().display()))?;
    if !cfg.envs.contains_key(src) {
        return Err(anyhow!("env '{src}' is not defined in rdc.toml"));
    }
    if !cfg.envs.contains_key(tgt) {
        return Err(anyhow!("env '{tgt}' is not defined in rdc.toml"));
    }

    let mapping_path = src_paths.mapping_file(src, tgt);
    let mut mapping = Mapping::load(&mapping_path)?;

    let h_new = match_kind(&mut mapping.hooks, &src_paths.hooks_dir(), &tgt_paths.hooks_dir())?;
    let r_new = match_kind(&mut mapping.rules, &src_paths.rules_dir(), &tgt_paths.rules_dir())?;
    let l_new = match_kind(&mut mapping.labels, &src_paths.labels_dir(), &tgt_paths.labels_dir())?;

    // Queue-nested kinds: walk every workspaces/<ws>/queues/<q>/ across both envs.
    let q_new = match_queues(&mut mapping.queues, &src_paths, &tgt_paths)?;
    let s_new = match_schemas(&mut mapping.schemas, &src_paths, &tgt_paths)?;
    let i_new = match_inboxes(&mut mapping.inboxes, &src_paths, &tgt_paths)?;
    let e_new = match_email_templates(&mut mapping.email_templates, &src_paths, &tgt_paths)?;

    // Other org-wide flat kinds. Workflows + workflow_steps are
    // intentionally excluded — Rossum's workflow API is read-only via PATCH
    // on every plan we've checked (OPTIONS returns "GET, HEAD, OPTIONS"),
    // so push/deploy can never succeed.
    let eng_new = match_kind(&mut mapping.engines, &src_paths.engines_dir(), &tgt_paths.engines_dir())?;
    let ef_new = match_kind(&mut mapping.engine_fields, &src_paths.engine_fields_dir(), &tgt_paths.engine_fields_dir())?;

    let any_total = mapping.hooks.len()
        + mapping.rules.len()
        + mapping.labels.len()
        + mapping.queues.len()
        + mapping.schemas.len()
        + mapping.inboxes.len()
        + mapping.email_templates.len()
        + mapping.engines.len()
        + mapping.engine_fields.len();
    if check {
        println!(
            "Would auto-match {h_new} new hooks, {r_new} new rules, {l_new} new labels, \
{q_new} new queues, {s_new} new schemas, {i_new} new inboxes, \
{e_new} new email templates, {eng_new} new engines, {ef_new} new engine fields \
by slug. Would write {}.",
            mapping_path.display()
        );
        return Ok(());
    }

    if any_total > 0 {
        std::fs::create_dir_all(src_paths.mapping_dir())
            .with_context(|| format!("creating {}", src_paths.mapping_dir().display()))?;
        mapping.save(&mapping_path)?;
    }

    println!(
        "Auto-matched {h_new} new hooks, {r_new} new rules, {l_new} new labels, \
{q_new} new queues, {s_new} new schemas, {i_new} new inboxes, \
{e_new} new email templates, {eng_new} new engines, {ef_new} new engine fields \
by slug. Wrote {}.",
        mapping_path.display()
    );
    Ok(())
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
