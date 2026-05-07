use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::paths::Paths;
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

    let mapping_path = src_paths.mapping_file(src, tgt);
    let mut mapping = Mapping::load(&mapping_path)?;

    let h_new = match_kind(&mut mapping.hooks, &src_paths.hooks_dir(), &tgt_paths.hooks_dir())?;
    let r_new = match_kind(&mut mapping.rules, &src_paths.rules_dir(), &tgt_paths.rules_dir())?;
    let l_new = match_kind(&mut mapping.labels, &src_paths.labels_dir(), &tgt_paths.labels_dir())?;

    let any_total = mapping.hooks.len() + mapping.rules.len() + mapping.labels.len();
    if any_total > 0 {
        std::fs::create_dir_all(src_paths.mapping_dir())
            .with_context(|| format!("creating {}", src_paths.mapping_dir().display()))?;
        mapping.save(&mapping_path)?;
    }

    println!(
        "Auto-matched {} new hooks, {} new rules, {} new labels by slug. Wrote {}.",
        h_new, r_new, l_new, mapping_path.display()
    );
    Ok(())
}

fn match_kind(
    existing: &mut BTreeMap<String, String>,
    src_dir: &std::path::Path,
    tgt_dir: &std::path::Path,
) -> Result<usize> {
    let src_slugs = list_slugs(src_dir)?;
    let tgt_slugs: std::collections::HashSet<_> = list_slugs(tgt_dir)?.into_iter().collect();
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

fn list_slugs(dir: &std::path::Path) -> Result<Vec<String>> {
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
