use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::paths::Paths;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

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

    let mut count = 0;
    let mut warnings = 0;
    for (src_slug, tgt_slug) in &mapping.hooks {
        let src_hook_path = src_paths.hooks_dir().join(format!("{src_slug}.json"));
        if !src_hook_path.exists() {
            eprintln!("warning: src hooks/{src_slug}.json missing — skipping in plan");
            warnings += 1;
            continue;
        }
        let tgt_id = tgt_lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(tgt_slug))
            .map(|e| e.id);
        let Some(tgt_id) = tgt_id else {
            eprintln!("warning: tgt lockfile has no entry for hooks/{tgt_slug} — run `rdc pull {tgt}` first");
            warnings += 1;
            continue;
        };
        println!("  ~ hooks/{src_slug}  →  {tgt}/{tgt_slug} (id {tgt_id})");
        count += 1;
    }

    if count == 0 && warnings == 0 {
        println!("  (no mapped hooks)");
    }
    Ok(())
}
