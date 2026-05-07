use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::paths::Paths;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub async fn run(src: &str, tgt: &str) -> Result<()> {
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

    let mapping = Mapping::load(&src_paths.mapping_file(src, tgt))?;
    let tgt_lockfile = Lockfile::load(&tgt_paths.lockfile())?;

    println!("Plan: {src} → {tgt}");

    let mut total_count = 0;
    let mut total_warnings = 0;
    total_count += plan_flat_kind(
        "hooks", &mapping.hooks,
        &src_paths.hooks_dir(),
        &tgt_lockfile, tgt, &mut total_warnings,
    );
    total_count += plan_flat_kind(
        "rules", &mapping.rules,
        &src_paths.rules_dir(),
        &tgt_lockfile, tgt, &mut total_warnings,
    );
    total_count += plan_flat_kind(
        "labels", &mapping.labels,
        &src_paths.labels_dir(),
        &tgt_lockfile, tgt, &mut total_warnings,
    );

    // Queue-nested kinds: src file lookup walks workspaces/<ws>/queues/<q>/.
    total_count += plan_queue_nested(
        "queues", &mapping.queues, &src_paths, "queue.json",
        &tgt_lockfile, tgt, &mut total_warnings,
    );
    total_count += plan_queue_nested(
        "schemas", &mapping.schemas, &src_paths, "schema.json",
        &tgt_lockfile, tgt, &mut total_warnings,
    );
    total_count += plan_queue_nested(
        "inboxes", &mapping.inboxes, &src_paths, "inbox.json",
        &tgt_lockfile, tgt, &mut total_warnings,
    );
    total_count += plan_email_templates(
        &mapping.email_templates, &src_paths,
        &tgt_lockfile, tgt, &mut total_warnings,
    );

    if total_count == 0 && total_warnings == 0 {
        println!("  (no mapped objects)");
    }
    Ok(())
}

fn plan_flat_kind(
    kind: &str,
    pairs: &BTreeMap<String, String>,
    src_dir: &Path,
    tgt_lockfile: &Lockfile,
    tgt: &str,
    warnings: &mut usize,
) -> usize {
    let mut count = 0;
    for (src_slug, tgt_slug) in pairs {
        let src_path = src_dir.join(format!("{src_slug}.json"));
        if !src_path.exists() {
            eprintln!("warning: src {kind}/{src_slug}.json missing — skipping in plan");
            *warnings += 1;
            continue;
        }
        let Some(tgt_id) = tgt_lockfile_id(tgt_lockfile, kind, tgt_slug) else {
            eprintln!("warning: tgt lockfile has no entry for {kind}/{tgt_slug} — run `rdc pull {tgt}` first");
            *warnings += 1;
            continue;
        };
        println!("  ~ {kind}/{src_slug}  →  {tgt}/{tgt_slug} (id {tgt_id})");
        count += 1;
    }
    count
}

fn plan_queue_nested(
    kind: &str,
    pairs: &BTreeMap<String, String>,
    src_paths: &Paths,
    file: &str,
    tgt_lockfile: &Lockfile,
    tgt: &str,
    warnings: &mut usize,
) -> usize {
    let mut count = 0;
    for (src_slug, tgt_slug) in pairs {
        let Some(src_path) = locate_queue_file(src_paths, src_slug, file) else {
            eprintln!("warning: src {kind} '{src_slug}' has no {file} on disk — skipping in plan");
            *warnings += 1;
            continue;
        };
        let _ = src_path;
        let Some(tgt_id) = tgt_lockfile_id(tgt_lockfile, kind, tgt_slug) else {
            eprintln!("warning: tgt lockfile has no entry for {kind}/{tgt_slug} — run `rdc pull {tgt}` first");
            *warnings += 1;
            continue;
        };
        println!("  ~ {kind}/{src_slug}  →  {tgt}/{tgt_slug} (id {tgt_id})");
        count += 1;
    }
    count
}

fn plan_email_templates(
    pairs: &BTreeMap<String, String>,
    src_paths: &Paths,
    tgt_lockfile: &Lockfile,
    tgt: &str,
    warnings: &mut usize,
) -> usize {
    let mut count = 0;
    for (src_key, tgt_key) in pairs {
        let Some((ws, q, t)) = split_template_key(src_key) else {
            eprintln!("warning: src email_template key '{src_key}' is not <ws>/<q>/<template>; skipping");
            *warnings += 1;
            continue;
        };
        let src_path = src_paths.queue_email_templates_dir(ws, q).join(format!("{t}.json"));
        if !src_path.exists() {
            eprintln!("warning: src email_template '{src_key}' missing on disk — skipping in plan");
            *warnings += 1;
            continue;
        }
        let Some(tgt_id) = tgt_lockfile_id(tgt_lockfile, "email_templates", tgt_key) else {
            eprintln!("warning: tgt lockfile has no entry for email_templates/{tgt_key} — run `rdc pull {tgt}` first");
            *warnings += 1;
            continue;
        };
        println!("  ~ email_templates/{src_key}  →  {tgt}/{tgt_key} (id {tgt_id})");
        count += 1;
    }
    count
}

fn tgt_lockfile_id(lockfile: &Lockfile, kind: &str, slug: &str) -> Option<u64> {
    lockfile
        .objects
        .get(kind)
        .and_then(|m| m.get(slug))
        .map(|e| e.id)
}

/// Find `<workspace>/queues/<q_slug>/<file>` for any workspace. Returns the
/// first match (queue slugs are unique-per-workspace; collisions across
/// workspaces are tolerated but rare in practice).
fn locate_queue_file(paths: &Paths, q_slug: &str, file: &str) -> Option<PathBuf> {
    let workspaces_dir = paths.workspaces_dir();
    if !workspaces_dir.exists() {
        return None;
    }
    let entries = std::fs::read_dir(&workspaces_dir).ok()?;
    for ws_entry in entries.flatten() {
        if !ws_entry.file_type().ok()?.is_dir() {
            continue;
        }
        let ws_slug = ws_entry.file_name().to_string_lossy().to_string();
        let candidate = paths.queue_dir(&ws_slug, q_slug).join(file);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn split_template_key(key: &str) -> Option<(&str, &str, &str)> {
    let mut parts = key.splitn(3, '/');
    let ws = parts.next()?;
    let q = parts.next()?;
    let t = parts.next()?;
    if ws.is_empty() || q.is_empty() || t.is_empty() {
        return None;
    }
    Some((ws, q, t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_template_key_three_parts() {
        assert_eq!(
            split_template_key("ws/q/t"),
            Some(("ws", "q", "t"))
        );
    }

    #[test]
    fn split_template_key_rejects_too_few() {
        assert!(split_template_key("ws/q").is_none());
        assert!(split_template_key("ws").is_none());
        assert!(split_template_key("").is_none());
    }

    #[test]
    fn split_template_key_three_parts_keeps_template_with_dashes() {
        let (_, _, t) = split_template_key("ws/q/annotation-status-change-confirmed").unwrap();
        assert_eq!(t, "annotation-status-change-confirmed");
    }
}
