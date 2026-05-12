use crate::api::{anyhow_has_status, RossumClient};
use crate::cli::deploy::common::{rewrite_urls, tgt_in_sync};
use crate::cli::pull::common::maybe_strip_overlay;
use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::snapshot::email_template::read_email_template;
use crate::snapshot::hook::read_hook_value;
use crate::snapshot::schema::read_schema_value;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
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
    let src_lockfile = Lockfile::load(&src_paths.lockfile())
        .with_context(|| format!("loading src lockfile from {}", src_paths.lockfile().display()))?;
    let tgt_lockfile = Lockfile::load(&tgt_paths.lockfile())?;
    let tgt_overlay = Overlay::load(&tgt_paths.overlay_file())
        .with_context(|| format!("loading tgt overlay from {}", tgt_paths.overlay_file().display()))?;

    let mut applied = ApplyCounts::default();
    let mut skipped = 0usize;

    // Hooks ------------------------------------------------------------
    for (src_slug, tgt_slug) in &mapping.hooks {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "hooks", tgt_slug, &mut skipped) else { continue };
        let mut payload = match read_hook_value(&src_paths.hooks_dir(), src_slug) {
            Ok(v) => v,
            Err(e) => { eprintln!("warning: cannot read src hooks/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.hook(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_hook: crate::model::Hook = match serde_json::from_value(payload.clone()) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("warning: hooks/{src_slug} → {tgt_slug}: payload not a valid Hook ({e:#}); skipping. Did you forget to set tgt overlay for required fields?");
                skipped += 1;
                continue;
            }
        };
        let (payload_json_full, payload_code) = crate::snapshot::hook::serialize_hook(&payload_hook)?;
        // Drift check.
        let remote_hook = tgt_client.get_hook(tgt_id, None).await
            .with_context(|| format!("fetching tgt hook {tgt_id} for drift check"))?;
        let (remote_json_full, remote_code) = crate::snapshot::hook::serialize_hook(&remote_hook)?;
        let in_sync = {
            let stripped = maybe_strip_overlay(remote_json_full.clone(), overlay_paths)?;
            let h = crate::state::hook_combined_hash(&stripped, &remote_code);
            let base = tgt_lockfile.objects.get("hooks").and_then(|m| m.get(tgt_slug)).and_then(|e| e.content_hash.as_deref());
            base.map(|b| b == h).unwrap_or(true)
        };
        if !in_sync {
            eprintln!("warning: tgt hooks/{tgt_slug} has drifted from tgt lockfile (run `rdc pull {tgt}` first); skipping");
            skipped += 1;
            continue;
        }
        // Idempotency.
        if payload_json_full == remote_json_full && payload_code == remote_code {
            continue;
        }
        // PATCH.
        tgt_client.update_hook(tgt_id, &payload_hook, None).await
            .with_context(|| format!("PATCH tgt hooks/{tgt_id} (mapped from src '{src_slug}')"))?;
        applied.hooks += 1;
    }

    // Rules ------------------------------------------------------------
    // Rules are a combined-hash kind (json + trigger_condition .py),
    // so the drift check and idempotency check both consider the
    // extracted code, not just the JSON bytes.
    let mut remote_rules_cache: Option<Vec<crate::model::Rule>> = None;
    for (src_slug, tgt_slug) in &mapping.rules {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "rules", tgt_slug, &mut skipped) else { continue };
        let mut payload = match crate::snapshot::rule::read_rule_value(&src_paths.rules_dir(), src_slug) {
            Ok(v) => v,
            Err(e) => { eprintln!("warning: cannot read src rules/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.rule(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_rule: crate::model::Rule = match serde_json::from_value(payload) {
            Ok(v) => v,
            Err(e) => { eprintln!("warning: rules/{src_slug} → {tgt_slug}: payload not a valid Rule ({e:#}); skipping"); skipped += 1; continue; }
        };
        let (payload_json_full, payload_code) = crate::snapshot::rule::serialize_rule(&payload_rule)?;

        if remote_rules_cache.is_none() {
            remote_rules_cache = Some(tgt_client.list_rules(None).await.context("listing tgt rules for drift check")?);
        }
        let cache = remote_rules_cache.as_ref().unwrap();
        let Some(remote) = cache.iter().find(|r| r.id == tgt_id) else {
            eprintln!("warning: rule id {tgt_id} not found on tgt remote; skipping");
            skipped += 1;
            continue;
        };
        let (remote_json_full, remote_code) = crate::snapshot::rule::serialize_rule(remote)?;
        let in_sync = {
            let stripped = maybe_strip_overlay(remote_json_full.clone(), overlay_paths)?;
            let h = crate::state::rule_combined_hash(&stripped, &remote_code);
            let base = tgt_lockfile.objects.get("rules").and_then(|m| m.get(tgt_slug)).and_then(|e| e.content_hash.as_deref());
            base.map(|b| b == h).unwrap_or(true)
        };
        if !in_sync {
            eprintln!("warning: tgt rules/{tgt_slug} has drifted from tgt lockfile (run `rdc pull {tgt}` first); skipping");
            skipped += 1;
            continue;
        }
        // Idempotency: both JSON and code must match.
        if payload_json_full == remote_json_full && payload_code == remote_code {
            continue;
        }
        tgt_client.update_rule(tgt_id, &payload_rule, None).await
            .with_context(|| format!("PATCH tgt rules/{tgt_id}"))?;
        applied.rules += 1;
    }

    // Labels -----------------------------------------------------------
    let mut remote_labels_cache: Option<Vec<crate::model::Label>> = None;
    for (src_slug, tgt_slug) in &mapping.labels {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "labels", tgt_slug, &mut skipped) else { continue };
        let path = src_paths.labels_dir().join(format!("{src_slug}.json"));
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => { eprintln!("warning: cannot read src labels/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => { eprintln!("warning: parsing labels/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.label(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_label: crate::model::Label = match serde_json::from_value(payload) {
            Ok(v) => v,
            Err(e) => { eprintln!("warning: labels/{src_slug} → {tgt_slug}: payload not a valid Label ({e:#}); skipping"); skipped += 1; continue; }
        };
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_label).context("serializing payload label")?;
        payload_bytes.push(b'\n');
        if remote_labels_cache.is_none() {
            remote_labels_cache = Some(tgt_client.list_labels(None).await.context("listing tgt labels for drift check")?);
        }
        let cache = remote_labels_cache.as_ref().unwrap();
        let Some(remote) = cache.iter().find(|l| l.id == tgt_id) else {
            eprintln!("warning: label id {tgt_id} not found on tgt remote; skipping");
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote).context("serializing remote label")?;
        remote_bytes.push(b'\n');
        if !tgt_in_sync(remote_bytes.clone(), overlay_paths, &tgt_lockfile, "labels", tgt_slug)? {
            eprintln!("warning: tgt labels/{tgt_slug} has drifted from tgt lockfile (run `rdc pull {tgt}` first); skipping");
            skipped += 1;
            continue;
        }
        if payload_bytes == remote_bytes {
            continue;
        }
        tgt_client.update_label(tgt_id, &payload_label, None).await
            .with_context(|| format!("PATCH tgt labels/{tgt_id}"))?;
        applied.labels += 1;
    }

    // Queues -----------------------------------------------------------
    let mut remote_queues_cache: Option<Vec<crate::model::Queue>> = None;
    for (src_slug, tgt_slug) in &mapping.queues {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "queues", tgt_slug, &mut skipped) else { continue };
        let Some(src_queue_dir) = locate_queue_dir(&src_paths, src_slug) else {
            eprintln!("warning: cannot locate src queue '{src_slug}' on disk — skipping");
            skipped += 1;
            continue;
        };
        let queue_path = src_queue_dir.join("queue.json");
        let raw = match std::fs::read_to_string(&queue_path) {
            Ok(r) => r,
            Err(e) => { eprintln!("warning: cannot read src queues/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => { eprintln!("warning: parsing queue '{src_slug}': {e:#}"); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.queue(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_queue: crate::model::Queue = match serde_json::from_value(payload) {
            Ok(v) => v,
            Err(e) => { eprintln!("warning: queues/{src_slug} → {tgt_slug}: payload not a valid Queue ({e:#}); skipping"); skipped += 1; continue; }
        };
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_queue).context("serializing payload queue")?;
        payload_bytes.push(b'\n');
        if remote_queues_cache.is_none() {
            remote_queues_cache = Some(tgt_client.list_queues(None).await.context("listing tgt queues for drift check")?);
        }
        let cache = remote_queues_cache.as_ref().unwrap();
        let Some(remote) = cache.iter().find(|q| q.id == tgt_id) else {
            eprintln!("warning: queue id {tgt_id} not found on tgt remote; skipping");
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote).context("serializing remote queue")?;
        remote_bytes.push(b'\n');
        if !tgt_in_sync(remote_bytes.clone(), overlay_paths, &tgt_lockfile, "queues", tgt_slug)? {
            eprintln!("warning: tgt queues/{tgt_slug} has drifted from tgt lockfile (run `rdc pull {tgt}` first); skipping");
            skipped += 1;
            continue;
        }
        if payload_bytes == remote_bytes {
            continue;
        }
        tgt_client.update_queue(tgt_id, &payload_queue, None).await
            .with_context(|| format!("PATCH tgt queues/{tgt_id}"))?;
        applied.queues += 1;
    }

    // Schemas ----------------------------------------------------------
    for (src_slug, tgt_slug) in &mapping.schemas {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "schemas", tgt_slug, &mut skipped) else { continue };
        let Some(src_queue_dir) = locate_queue_dir(&src_paths, src_slug) else {
            eprintln!("warning: cannot locate src queue '{src_slug}' for schema — skipping");
            skipped += 1;
            continue;
        };
        let mut payload = match read_schema_value(&src_queue_dir) {
            Ok(v) => v,
            Err(e) => { eprintln!("warning: cannot read src schema for queue '{src_slug}': {e:#}"); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.schema(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_schema: crate::model::Schema = match serde_json::from_value(payload) {
            Ok(s) => s,
            Err(e) => { eprintln!("warning: schemas/{src_slug} → {tgt_slug}: payload not a valid Schema ({e:#}); skipping"); skipped += 1; continue; }
        };
        let (payload_json_full, payload_formulas) =
            crate::snapshot::schema::serialize_schema(&payload_schema)?;
        let remote_schema = tgt_client.get_schema(tgt_id, None).await
            .with_context(|| format!("fetching tgt schema {tgt_id} for drift check"))?;
        let (remote_json_full, remote_formulas) =
            crate::snapshot::schema::serialize_schema(&remote_schema)?;
        let in_sync = {
            let stripped = maybe_strip_overlay(remote_json_full.clone(), overlay_paths)?;
            let h = crate::state::schema_combined_hash(&stripped, &remote_formulas);
            let base = tgt_lockfile.objects.get("schemas").and_then(|m| m.get(tgt_slug)).and_then(|e| e.content_hash.as_deref());
            base.map(|b| b == h).unwrap_or(true)
        };
        if !in_sync {
            eprintln!("warning: tgt schemas/{tgt_slug} has drifted from tgt lockfile (run `rdc pull {tgt}` first); skipping");
            skipped += 1;
            continue;
        }
        if payload_json_full == remote_json_full && payload_formulas == remote_formulas {
            continue;
        }
        tgt_client.update_schema(tgt_id, &payload_schema, None).await
            .with_context(|| format!("PATCH tgt schemas/{tgt_id}"))?;
        applied.schemas += 1;
    }

    // Inboxes ----------------------------------------------------------
    for (src_slug, tgt_slug) in &mapping.inboxes {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "inboxes", tgt_slug, &mut skipped) else { continue };
        let Some(src_queue_dir) = locate_queue_dir(&src_paths, src_slug) else {
            eprintln!("warning: cannot locate src queue '{src_slug}' for inbox — skipping");
            skipped += 1;
            continue;
        };
        let inbox_path = src_queue_dir.join("inbox.json");
        let raw = match std::fs::read_to_string(&inbox_path) {
            Ok(r) => r,
            Err(e) => { eprintln!("warning: cannot read src inbox for queue '{src_slug}': {e:#}"); skipped += 1; continue; }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => { eprintln!("warning: parsing inbox for queue '{src_slug}': {e:#}"); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.inbox(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_inbox: crate::model::Inbox = match serde_json::from_value(payload) {
            Ok(i) => i,
            Err(e) => { eprintln!("warning: inboxes/{src_slug} → {tgt_slug}: payload not a valid Inbox ({e:#}); skipping"); skipped += 1; continue; }
        };
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_inbox).context("serializing payload inbox")?;
        payload_bytes.push(b'\n');
        let remote_inbox = tgt_client.get_inbox(tgt_id, None).await
            .with_context(|| format!("fetching tgt inbox {tgt_id} for drift check"))?;
        let mut remote_bytes = serde_json::to_vec_pretty(&remote_inbox).context("serializing remote inbox")?;
        remote_bytes.push(b'\n');
        if !tgt_in_sync(remote_bytes.clone(), overlay_paths, &tgt_lockfile, "inboxes", tgt_slug)? {
            eprintln!("warning: tgt inboxes/{tgt_slug} has drifted from tgt lockfile (run `rdc pull {tgt}` first); skipping");
            skipped += 1;
            continue;
        }
        if payload_bytes == remote_bytes {
            continue;
        }
        tgt_client.update_inbox(tgt_id, &payload_inbox, None).await
            .with_context(|| format!("PATCH tgt inboxes/{tgt_id}"))?;
        applied.inboxes += 1;
    }

    // Email templates --------------------------------------------------
    let mut remote_template_cache: Option<Vec<crate::model::EmailTemplate>> = None;
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
        let mut payload = serde_json::to_value(&src_template).context("serializing src email template to value")?;
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.email_template(tgt_key));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_template: crate::model::EmailTemplate = match serde_json::from_value(payload) {
            Ok(t) => t,
            Err(e) => { eprintln!("warning: email_templates/{src_key} → {tgt_key}: payload not a valid EmailTemplate ({e:#}); skipping"); skipped += 1; continue; }
        };
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_template).context("serializing payload email template")?;
        payload_bytes.push(b'\n');
        if remote_template_cache.is_none() {
            remote_template_cache = Some(tgt_client.list_email_templates(None).await
                .context("listing tgt email templates for drift check")?);
        }
        let cache = remote_template_cache.as_ref().unwrap();
        let Some(remote_template) = cache.iter().find(|t| t.id == tgt_id) else {
            eprintln!("warning: email_template id {tgt_id} not found on tgt remote; skipping");
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote_template).context("serializing remote email template")?;
        remote_bytes.push(b'\n');
        if !tgt_in_sync(remote_bytes.clone(), overlay_paths, &tgt_lockfile, "email_templates", tgt_key)? {
            eprintln!("warning: tgt email_templates/{tgt_key} has drifted from tgt lockfile (run `rdc pull {tgt}` first); skipping");
            skipped += 1;
            continue;
        }
        if payload_bytes == remote_bytes {
            continue;
        }
        tgt_client.update_email_template(tgt_id, &payload_template, None).await
            .with_context(|| format!("PATCH tgt email_templates/{tgt_id}"))?;
        applied.email_templates += 1;
    }

    // Engines ----------------------------------------------------------
    for (src_slug, tgt_slug) in &mapping.engines {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "engines", tgt_slug, &mut skipped) else { continue };
        let path = src_paths.engines_dir().join(format!("{src_slug}.json"));
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => { eprintln!("warning: cannot read src engines/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => { eprintln!("warning: parsing engines/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.engine(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_engine: crate::model::Engine = match serde_json::from_value(payload) {
            Ok(e) => e,
            Err(e) => { eprintln!("warning: engines/{src_slug} → {tgt_slug}: payload not a valid Engine ({e:#}); skipping"); skipped += 1; continue; }
        };
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_engine).context("serializing payload engine")?;
        payload_bytes.push(b'\n');
        let remotes = tgt_client.list_engines(None).await.context("listing tgt engines for drift check")?;
        let Some(remote) = remotes.iter().find(|e| e.id == tgt_id) else {
            eprintln!("warning: engine id {tgt_id} not found on tgt remote; skipping");
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote).context("serializing remote engine")?;
        remote_bytes.push(b'\n');
        if !tgt_in_sync(remote_bytes.clone(), overlay_paths, &tgt_lockfile, "engines", tgt_slug)? {
            eprintln!("warning: tgt engines/{tgt_slug} has drifted from tgt lockfile (run `rdc pull {tgt}` first); skipping");
            skipped += 1;
            continue;
        }
        if payload_bytes == remote_bytes {
            continue;
        }
        match tgt_client.update_engine(tgt_id, &payload_engine, None).await
            .with_context(|| format!("PATCH tgt engines/{tgt_id}"))
        {
            Ok(_) => applied.engines += 1,
            Err(e) if anyhow_has_status(&e, 405) => {
                eprintln!("warning: engines are not writable via PATCH on tgt org/plan (405). Skipping all engine apply.");
                skipped += 1;
                break;
            }
            Err(e) => return Err(e),
        }
    }

    // Engine fields ----------------------------------------------------
    for (src_slug, tgt_slug) in &mapping.engine_fields {
        let Some(tgt_id) = lookup_tgt_id(&tgt_lockfile, "engine_fields", tgt_slug, &mut skipped) else { continue };
        let Some(path) = locate_engine_field_path(&src_paths, src_slug) else {
            eprintln!(
                "warning: cannot locate src engine field '{src_slug}' under any engine dir; skipping"
            );
            skipped += 1;
            continue;
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => { eprintln!("warning: cannot read src engine field '{src_slug}': {e:#}"); skipped += 1; continue; }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => { eprintln!("warning: parsing engine-fields/{src_slug}: {e:#}"); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.engine_field(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_field: crate::model::EngineField = match serde_json::from_value(payload) {
            Ok(f) => f,
            Err(e) => { eprintln!("warning: engine-fields/{src_slug} → {tgt_slug}: payload not a valid EngineField ({e:#}); skipping"); skipped += 1; continue; }
        };
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_field).context("serializing payload engine field")?;
        payload_bytes.push(b'\n');
        let remotes = tgt_client.list_engine_fields(None).await.context("listing tgt engine fields for drift check")?;
        let Some(remote) = remotes.iter().find(|f| f.id == tgt_id) else {
            eprintln!("warning: engine_field id {tgt_id} not found on tgt remote; skipping");
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote).context("serializing remote engine field")?;
        remote_bytes.push(b'\n');
        if !tgt_in_sync(remote_bytes.clone(), overlay_paths, &tgt_lockfile, "engine_fields", tgt_slug)? {
            eprintln!("warning: tgt engine-fields/{tgt_slug} has drifted from tgt lockfile (run `rdc pull {tgt}` first); skipping");
            skipped += 1;
            continue;
        }
        if payload_bytes == remote_bytes {
            continue;
        }
        match tgt_client.update_engine_field(tgt_id, &payload_field, None).await
            .with_context(|| format!("PATCH tgt engine_fields/{tgt_id}"))
        {
            Ok(_) => applied.engine_fields += 1,
            Err(e) if anyhow_has_status(&e, 405) => {
                eprintln!("warning: engine fields are not writable via PATCH on tgt org/plan (405). Skipping all engine field apply.");
                skipped += 1;
                break;
            }
            Err(e) => return Err(e),
        }
    }

    let total = applied.total();
    let mut summary = format!(
        "Applied {} hooks, {} rules, {} labels, {} queues, {} schemas, {} inboxes, \
{} email templates, {} engines, {} engine fields ({} PATCHes) from {src} to {tgt}",
        applied.hooks, applied.rules, applied.labels, applied.queues,
        applied.schemas, applied.inboxes, applied.email_templates,
        applied.engines, applied.engine_fields, total,
    );
    if skipped > 0 {
        summary.push_str(&format!(", {skipped} skipped"));
    }
    println!("{summary}");
    Ok(())
}

#[derive(Default)]
struct ApplyCounts {
    hooks: usize,
    rules: usize,
    labels: usize,
    queues: usize,
    schemas: usize,
    inboxes: usize,
    email_templates: usize,
    engines: usize,
    engine_fields: usize,
}
impl ApplyCounts {
    fn total(&self) -> usize {
        self.hooks + self.rules + self.labels + self.queues + self.schemas
            + self.inboxes + self.email_templates + self.engines + self.engine_fields
    }
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

/// Locate an engine field's on-disk path by walking
/// `engines/*/fields/<field_slug>.json`. Returns the first match
/// (engine_fields slugs are globally unique).
fn locate_engine_field_path(paths: &Paths, field_slug: &str) -> Option<PathBuf> {
    let engines_dir = paths.engines_dir();
    let entries = std::fs::read_dir(&engines_dir).ok()?;
    for e_entry in entries.flatten() {
        if !e_entry.file_type().ok()?.is_dir() {
            continue;
        }
        let e_slug = e_entry.file_name().to_string_lossy().to_string();
        let candidate = paths.engine_fields_dir(&e_slug).join(format!("{field_slug}.json"));
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
