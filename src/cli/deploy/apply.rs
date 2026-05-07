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

    let mut applied_hooks = 0;
    let mut applied_rules = 0;
    let mut applied_labels = 0;
    let mut skipped = 0;

    for (src_slug, tgt_slug) in &mapping.hooks {
        let tgt_id = match tgt_lockfile.objects.get("hooks").and_then(|m| m.get(tgt_slug)).map(|e| e.id) {
            Some(id) => id,
            None => { eprintln!("warning: tgt lockfile has no entry for hooks/{tgt_slug} — skipping"); skipped += 1; continue; }
        };
        let src_hook = match read_hook(&src_paths.hooks_dir(), src_slug) {
            Ok(h) => h,
            Err(e) => { eprintln!("warning: cannot read src hooks/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        let mut payload = serde_json::to_value(&src_hook).context("serializing src hook")?;
        if let Some(ov) = &tgt_overlay {
            if let Some(overrides) = ov.hook(tgt_slug) {
                apply_overrides(&mut payload, overrides);
            }
        }
        let payload_hook: crate::model::Hook = serde_json::from_value(payload)
            .with_context(|| format!("re-deserializing overlay-applied hook for tgt slug '{tgt_slug}'"))?;
        tgt_client.update_hook(tgt_id, &payload_hook).await
            .with_context(|| format!("PATCH tgt hooks/{tgt_id} (mapped from src '{src_slug}')"))?;
        applied_hooks += 1;
    }

    for (src_slug, tgt_slug) in &mapping.rules {
        let tgt_id = match tgt_lockfile.objects.get("rules").and_then(|m| m.get(tgt_slug)).map(|e| e.id) {
            Some(id) => id,
            None => { eprintln!("warning: tgt lockfile has no entry for rules/{tgt_slug} — skipping"); skipped += 1; continue; }
        };
        let path = src_paths.rules_dir().join(format!("{src_slug}.json"));
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => { eprintln!("warning: cannot read src rules/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        let src_rule: crate::model::Rule = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        let mut payload = serde_json::to_value(&src_rule).context("serializing src rule")?;
        if let Some(ov) = &tgt_overlay {
            if let Some(overrides) = ov.rule(tgt_slug) {
                apply_overrides(&mut payload, overrides);
            }
        }
        let payload_rule: crate::model::Rule = serde_json::from_value(payload)
            .with_context(|| format!("re-deserializing overlay-applied rule for tgt slug '{tgt_slug}'"))?;
        tgt_client.update_rule(tgt_id, &payload_rule).await
            .with_context(|| format!("PATCH tgt rules/{tgt_id}"))?;
        applied_rules += 1;
    }

    for (src_slug, tgt_slug) in &mapping.labels {
        let tgt_id = match tgt_lockfile.objects.get("labels").and_then(|m| m.get(tgt_slug)).map(|e| e.id) {
            Some(id) => id,
            None => { eprintln!("warning: tgt lockfile has no entry for labels/{tgt_slug} — skipping"); skipped += 1; continue; }
        };
        let path = src_paths.labels_dir().join(format!("{src_slug}.json"));
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => { eprintln!("warning: cannot read src labels/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        let src_label: crate::model::Label = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        let mut payload = serde_json::to_value(&src_label).context("serializing src label")?;
        if let Some(ov) = &tgt_overlay {
            if let Some(overrides) = ov.label(tgt_slug) {
                apply_overrides(&mut payload, overrides);
            }
        }
        let payload_label: crate::model::Label = serde_json::from_value(payload)
            .with_context(|| format!("re-deserializing overlay-applied label for tgt slug '{tgt_slug}'"))?;
        tgt_client.update_label(tgt_id, &payload_label).await
            .with_context(|| format!("PATCH tgt labels/{tgt_id}"))?;
        applied_labels += 1;
    }

    let total = applied_hooks + applied_rules + applied_labels;
    let mut summary = format!(
        "Applied {} hooks, {} rules, {} labels ({} PATCHes) from {src} to {tgt}",
        applied_hooks, applied_rules, applied_labels, total
    );
    if skipped > 0 {
        summary.push_str(&format!(", {} skipped", skipped));
    }
    println!("{summary}");
    Ok(())
}
