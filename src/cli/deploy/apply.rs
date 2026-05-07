use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::snapshot::hook::read_hook;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};

pub async fn run(src: &str, tgt: &str) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let src_paths = Paths::for_env(&cwd, src);
    let tgt_paths = Paths::for_env(&cwd, tgt);

    let cfg = ProjectConfig::load(&src_paths.project_config())
        .with_context(|| format!("loading project config from {}", src_paths.project_config().display()))?;
    let _src_cfg = cfg.envs.get(src).ok_or_else(|| anyhow!("env '{src}' is not defined in rdc.toml"))?;
    let tgt_cfg = cfg.envs.get(tgt).ok_or_else(|| anyhow!("env '{tgt}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, tgt)?;
    let tgt_client = RossumClient::new(tgt_cfg.api_base.clone(), token)
        .context("constructing tgt API client")?;

    let mapping = Mapping::load(&src_paths.mapping_file(src, tgt))?;
    let tgt_lockfile = Lockfile::load(&tgt_paths.lockfile())?;
    let tgt_overlay = Overlay::load(&tgt_paths.overlay_file())
        .with_context(|| format!("loading tgt overlay from {}", tgt_paths.overlay_file().display()))?;

    let mut applied = 0;
    let mut skipped = 0;

    for (src_slug, tgt_slug) in &mapping.hooks {
        let tgt_id = tgt_lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(tgt_slug))
            .map(|e| e.id);
        let Some(tgt_id) = tgt_id else {
            eprintln!("warning: tgt lockfile has no entry for hooks/{tgt_slug} — skipping");
            skipped += 1;
            continue;
        };

        let src_hook = match read_hook(&src_paths.hooks_dir(), src_slug) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("warning: cannot read src hooks/{src_slug}: {e:#}");
                skipped += 1;
                continue;
            }
        };

        let mut payload = serde_json::to_value(&src_hook)
            .context("serializing src hook to value")?;
        if let Some(ov) = &tgt_overlay {
            if let Some(hook_overrides) = ov.hook(tgt_slug) {
                apply_overrides(&mut payload, hook_overrides);
            }
        }
        let payload_hook: crate::model::Hook = serde_json::from_value(payload)
            .with_context(|| format!("re-deserializing overlay-applied hook for tgt slug '{tgt_slug}'"))?;

        tgt_client.update_hook(tgt_id, &payload_hook).await
            .with_context(|| format!("PATCH tgt hooks/{tgt_id} (mapped from src '{src_slug}')"))?;
        applied += 1;
    }

    let mut summary = format!("Applied {applied} hook PATCHes from {src} to {tgt}");
    if skipped > 0 {
        summary.push_str(&format!(", {skipped} skipped"));
    }
    println!("{summary}");
    Ok(())
}
