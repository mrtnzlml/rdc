use crate::api::RossumClient;
use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::snapshot::email_template::read_email_template;
use crate::snapshot::hook::read_hook;
use crate::snapshot::inbox::read_inbox;
use crate::snapshot::queue::read_queue;
use crate::snapshot::schema::read_schema;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

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

    let mut applied_hooks = 0usize;
    let mut applied_rules = 0usize;
    let mut applied_labels = 0usize;
    let mut applied_queues = 0usize;
    let mut applied_schemas = 0usize;
    let mut applied_inboxes = 0usize;
    let mut applied_email_templates = 0usize;
    let mut skipped = 0usize;

    // Hooks (M12)
    for (src_slug, tgt_slug) in &mapping.hooks {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "hooks", tgt_slug, &mut skipped) else { continue };
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

    // Rules (M13)
    for (src_slug, tgt_slug) in &mapping.rules {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "rules", tgt_slug, &mut skipped) else { continue };
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

    // Labels (M13)
    for (src_slug, tgt_slug) in &mapping.labels {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "labels", tgt_slug, &mut skipped) else { continue };
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

    // Queues (M19)
    for (src_slug, tgt_slug) in &mapping.queues {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "queues", tgt_slug, &mut skipped) else { continue };
        let Some(src_queue_dir) = locate_queue_dir(&src_paths, src_slug) else {
            eprintln!("warning: cannot locate src queue '{src_slug}' on disk — skipping");
            skipped += 1;
            continue;
        };
        let src_queue = match read_queue(&src_queue_dir) {
            Ok(q) => q,
            Err(e) => { eprintln!("warning: cannot read src queues/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        let mut payload = serde_json::to_value(&src_queue).context("serializing src queue")?;
        if let Some(ov) = &tgt_overlay {
            if let Some(overrides) = ov.queue(tgt_slug) {
                apply_overrides(&mut payload, overrides);
            }
        }
        let payload_queue: crate::model::Queue = serde_json::from_value(payload)
            .with_context(|| format!("re-deserializing overlay-applied queue for tgt slug '{tgt_slug}'"))?;
        tgt_client.update_queue(tgt_id, &payload_queue).await
            .with_context(|| format!("PATCH tgt queues/{tgt_id}"))?;
        applied_queues += 1;
    }

    // Schemas (M19) — read with formula splice.
    for (src_slug, tgt_slug) in &mapping.schemas {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "schemas", tgt_slug, &mut skipped) else { continue };
        let Some(src_queue_dir) = locate_queue_dir(&src_paths, src_slug) else {
            eprintln!("warning: cannot locate src queue '{src_slug}' for schema — skipping");
            skipped += 1;
            continue;
        };
        let src_schema = match read_schema(&src_queue_dir) {
            Ok(s) => s,
            Err(e) => { eprintln!("warning: cannot read src schema for queue '{src_slug}': {e:#}"); skipped += 1; continue; }
        };
        let mut payload = serde_json::to_value(&src_schema).context("serializing src schema")?;
        if let Some(ov) = &tgt_overlay {
            if let Some(overrides) = ov.schema(tgt_slug) {
                apply_overrides(&mut payload, overrides);
            }
        }
        let payload_schema: crate::model::Schema = serde_json::from_value(payload)
            .with_context(|| format!("re-deserializing overlay-applied schema for tgt slug '{tgt_slug}'"))?;
        tgt_client.update_schema(tgt_id, &payload_schema).await
            .with_context(|| format!("PATCH tgt schemas/{tgt_id}"))?;
        applied_schemas += 1;
    }

    // Inboxes (M19)
    for (src_slug, tgt_slug) in &mapping.inboxes {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "inboxes", tgt_slug, &mut skipped) else { continue };
        let Some(src_queue_dir) = locate_queue_dir(&src_paths, src_slug) else {
            eprintln!("warning: cannot locate src queue '{src_slug}' for inbox — skipping");
            skipped += 1;
            continue;
        };
        let src_inbox = match read_inbox(&src_queue_dir) {
            Ok(i) => i,
            Err(e) => { eprintln!("warning: cannot read src inbox for queue '{src_slug}': {e:#}"); skipped += 1; continue; }
        };
        let mut payload = serde_json::to_value(&src_inbox).context("serializing src inbox")?;
        if let Some(ov) = &tgt_overlay {
            if let Some(overrides) = ov.inbox(tgt_slug) {
                apply_overrides(&mut payload, overrides);
            }
        }
        let payload_inbox: crate::model::Inbox = serde_json::from_value(payload)
            .with_context(|| format!("re-deserializing overlay-applied inbox for tgt slug '{tgt_slug}'"))?;
        tgt_client.update_inbox(tgt_id, &payload_inbox).await
            .with_context(|| format!("PATCH tgt inboxes/{tgt_id}"))?;
        applied_inboxes += 1;
    }

    // Email templates (M19)
    for (src_key, tgt_key) in &mapping.email_templates {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "email_templates", tgt_key, &mut skipped) else { continue };
        let Some((ws, q, t)) = split_template_key(src_key) else {
            eprintln!("warning: src email_template key '{src_key}' is not <ws>/<q>/<template>; skipping");
            skipped += 1;
            continue;
        };
        let templates_dir = src_paths.queue_email_templates_dir(ws, q);
        let src_template = match read_email_template(&templates_dir, t) {
            Ok(t) => t,
            Err(e) => { eprintln!("warning: cannot read src email_template '{src_key}': {e:#}"); skipped += 1; continue; }
        };
        let mut payload = serde_json::to_value(&src_template).context("serializing src email template")?;
        if let Some(ov) = &tgt_overlay {
            if let Some(overrides) = ov.email_template(tgt_key) {
                apply_overrides(&mut payload, overrides);
            }
        }
        let payload_template: crate::model::EmailTemplate = serde_json::from_value(payload)
            .with_context(|| format!("re-deserializing overlay-applied email template for tgt key '{tgt_key}'"))?;
        tgt_client.update_email_template(tgt_id, &payload_template).await
            .with_context(|| format!("PATCH tgt email_templates/{tgt_id}"))?;
        applied_email_templates += 1;
    }

    let total = applied_hooks + applied_rules + applied_labels
        + applied_queues + applied_schemas + applied_inboxes + applied_email_templates;
    let mut summary = format!(
        "Applied {applied_hooks} hooks, {applied_rules} rules, {applied_labels} labels, \
{applied_queues} queues, {applied_schemas} schemas, {applied_inboxes} inboxes, \
{applied_email_templates} email templates ({total} PATCHes) from {src} to {tgt}"
    );
    if skipped > 0 {
        summary.push_str(&format!(", {skipped} skipped"));
    }
    println!("{summary}");
    Ok(())
}

fn lookup_tgt_id(
    tgt_lockfile: &Lockfile,
    kind: &str,
    tgt_slug: &str,
    skipped: &mut usize,
) -> Option<u64> {
    match tgt_lockfile.objects.get(kind).and_then(|m| m.get(tgt_slug)).map(|e| e.id) {
        Some(id) => Some(id),
        None => {
            eprintln!(
                "warning: tgt lockfile has no entry for {kind}/{tgt_slug} — skipping (run `rdc pull <tgt>` first)"
            );
            *skipped += 1;
            None
        }
    }
}

fn locate_queue_dir(paths: &Paths, q_slug: &str) -> Option<PathBuf> {
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
        let candidate = paths.queue_dir(&ws_slug, q_slug);
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
