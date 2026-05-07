use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::paths::Paths;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;

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
    total_count += plan_kind("hooks", &mapping.hooks, &src_paths.hooks_dir(), &tgt_lockfile, tgt, &mut total_warnings);
    total_count += plan_kind("rules", &mapping.rules, &src_paths.rules_dir(), &tgt_lockfile, tgt, &mut total_warnings);
    total_count += plan_kind("labels", &mapping.labels, &src_paths.labels_dir(), &tgt_lockfile, tgt, &mut total_warnings);

    if total_count == 0 && total_warnings == 0 {
        println!("  (no mapped objects)");
    }
    Ok(())
}

fn plan_kind(
    kind: &str,
    pairs: &BTreeMap<String, String>,
    src_dir: &std::path::Path,
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
        let tgt_id = tgt_lockfile
            .objects
            .get(kind)
            .and_then(|m| m.get(tgt_slug))
            .map(|e| e.id);
        let Some(tgt_id) = tgt_id else {
            eprintln!("warning: tgt lockfile has no entry for {kind}/{tgt_slug} — run `rdc pull {tgt}` first");
            *warnings += 1;
            continue;
        };
        println!("  ~ {kind}/{src_slug}  →  {tgt}/{tgt_slug} (id {tgt_id})");
        count += 1;
    }
    count
}
