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

    let src_hooks = list_hook_slugs(&src_paths.hooks_dir())?;
    let tgt_hooks = list_hook_slugs(&tgt_paths.hooks_dir())?;
    let tgt_set: std::collections::HashSet<_> = tgt_hooks.iter().cloned().collect();

    let mapping_path = src_paths.mapping_file(src, tgt);
    let mut mapping = Mapping::load(&mapping_path)?;

    let pre_count = mapping.hooks.len();
    let mut newly_matched: BTreeMap<String, String> = BTreeMap::new();
    for src_slug in &src_hooks {
        if mapping.hooks.contains_key(src_slug) {
            continue;
        }
        if tgt_set.contains(src_slug) {
            newly_matched.insert(src_slug.clone(), src_slug.clone());
        }
    }
    let new_count = newly_matched.len();
    mapping.hooks.extend(newly_matched);

    if !mapping.hooks.is_empty() {
        std::fs::create_dir_all(src_paths.mapping_dir())
            .with_context(|| format!("creating {}", src_paths.mapping_dir().display()))?;
        mapping.save(&mapping_path)?;
    }

    println!(
        "Auto-matched {} new hooks by slug ({} total in mapping). Wrote {}.",
        new_count,
        pre_count + new_count,
        mapping_path.display()
    );
    Ok(())
}

fn list_hook_slugs(hooks_dir: &std::path::Path) -> Result<Vec<String>> {
    if !hooks_dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(hooks_dir)
        .with_context(|| format!("reading {}", hooks_dir.display()))?
    {
        let entry = entry
            .with_context(|| format!("listing {}", hooks_dir.display()))?;
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
