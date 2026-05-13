use crate::api::{anyhow_has_status, RossumClient};
use crate::cli::deploy::common::{bytes_equal_after_strip, rewrite_urls, tgt_drift_status};
use crate::snapshot::create::strip_for_cross_env_patch;
use crate::cli::pull::common::maybe_strip_overlay;
use crate::config::ProjectConfig;
use crate::mapping::Mapping;
use crate::overlay::{apply_overrides, Overlay};
use crate::paths::Paths;
use crate::progress::OverallProgress;
use crate::secrets::resolve_token;
use crate::snapshot::email_template::read_email_template;
use crate::snapshot::hook::read_hook_value;
use crate::snapshot::schema::read_schema_value;
use crate::state::Lockfile;
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// Emit a warning through the progress bar if active, or to stderr directly.
fn warn(progress: &Option<Arc<OverallProgress>>, msg: String) {
    match progress {
        Some(p) => p.println(&msg),
        None => eprintln!("{msg}"),
    }
}

/// Cheap check on a raw payload: is this a Rossum-store hook? Used to
/// decide whether to pin `token_owner` from the tgt overlay during the
/// cross-env update phase.
fn is_store_extension(payload: &Value) -> bool {
    payload
        .get("extension_source")
        .and_then(|v| v.as_str())
        == Some("rossum_store")
}

/// Adopt an out-of-band change on tgt: refresh the lockfile entry's
/// `content_hash` to the current remote hash and emit a quiet note.
/// Used when the drift check detects that someone modified the tgt
/// object directly (e.g. via the Rossum UI) since the last pull — the
/// deploy proceeds anyway, treating the remote-as-of-now as the new
/// baseline that the upcoming PATCH overwrites.
fn adopt_tgt_drift(
    progress: &Option<Arc<OverallProgress>>,
    tgt_lockfile: &mut Lockfile,
    kind: &str,
    tgt_slug: &str,
    remote_hash: String,
) {
    if let Some(entry) = tgt_lockfile
        .objects
        .get_mut(kind)
        .and_then(|m| m.get_mut(tgt_slug))
    {
        entry.content_hash = Some(remote_hash);
    }
    let msg =
        format!("note: tgt {kind}/{tgt_slug} had out-of-band changes; adopting as new baseline");
    match progress {
        Some(p) => p.println(&msg),
        None => eprintln!("{msg}"),
    }
}

/// Drive the cross-env update phase.
///
/// `dry_run = true` traces the same code path (URL rewrite, overlay,
/// idempotency check) but skips every actual PATCH/POST. Used by
/// `rdc deploy --dry-run` to surface what *would* change without
/// touching the target. The printed summary swaps "Applied" for
/// "Would apply" in that mode.
///
/// `diff = true` (only meaningful with `dry_run = true`) prints a
/// unified diff per object whose canonical content differs between src
/// (after URL rewrite + overlay) and tgt remote.
///
/// Returns the summary line as a `String` so the caller can print it
/// after the progress bar is finished.
pub async fn run(
    src: &str,
    tgt: &str,
    dry_run: bool,
    diff: bool,
    progress: Option<Arc<OverallProgress>>,
    tgt_lockfile: &mut Lockfile,
) -> Result<String> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let src_paths = Paths::for_env(&cwd, src);
    let tgt_paths = Paths::for_env(&cwd, tgt);

    let cfg = ProjectConfig::load(&src_paths.project_config())?;
    let _src_cfg = cfg.envs.get(src).ok_or_else(|| anyhow!("env '{src}' is not defined in rdc.toml"))?;
    let tgt_cfg = cfg.envs.get(tgt).ok_or_else(|| anyhow!("env '{tgt}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, tgt)?;
    let tgt_client = RossumClient::new(tgt_cfg.api_base.clone(), token)
        .context("constructing tgt API client")?;

    let mapping = Mapping::load(&src_paths.mapping_file(src, tgt))?;
    let src_lockfile = Lockfile::load(&src_paths.lockfile())
        .with_context(|| format!("loading src lockfile from {}", src_paths.lockfile().display()))?;
    let tgt_overlay = Overlay::load(&tgt_paths.overlay_file())
        .with_context(|| format!("loading tgt overlay from {}", tgt_paths.overlay_file().display()))?;

    let mut applied = ApplyCounts::default();
    let mut skipped = 0usize;
    let empty_subs: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();

    // Hooks ------------------------------------------------------------
    if !mapping.hooks.is_empty() {
        progress.as_ref().map(|p| {
            p.inc_total(mapping.hooks.len() as u64);
            p.start_phase("update hooks");
        });
    }
    for (src_slug, tgt_slug) in &mapping.hooks {
        let Some(tgt_id) = lookup_tgt_id_w(&tgt_lockfile, "hooks", tgt_slug, &mut skipped, &progress) else { continue };
        let mut payload = match read_hook_value(&src_paths.hooks_dir(), src_slug) {
            Ok(v) => v,
            Err(e) => { warn(&progress, format!("warning: cannot read src hooks/{src_slug}: {e:#}")); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping, &empty_subs);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.hook(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        // `token_owner` is a tgt-env user URL; the src snapshot carries
        // the src-env user URL, which the tgt API rejects as "Invalid
        // hyperlink". For store extensions, always pin it to the
        // effective overlay value (per-hook → `[defaults]`) — that's
        // the URL the user picked during bootstrap. If neither is set,
        // refuse to PATCH and tell the user how to fix it.
        if is_store_extension(&payload) {
            match crate::cli::deploy::store_extensions::effective_token_owner(
                tgt_overlay.as_ref(),
                tgt_slug,
            ) {
                Some(url) => {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert(
                            "token_owner".into(),
                            serde_json::Value::String(url.to_string()),
                        );
                    }
                }
                None => {
                    warn(&progress, format!(
                        "warning: hooks/{tgt_slug} is a store extension but {} has no \
                         token_owner for it (neither [hooks.{tgt_slug}] token_owner nor \
                         [defaults] store_extension_token_owner). Run `rdc deploy {src} {tgt}` \
                         on a TTY once to pick interactively, or set it in the overlay file \
                         directly. Skipping this hook.",
                        tgt_paths.overlay_file().display(),
                    ));
                    skipped += 1;
                    continue;
                }
            }
        }
        let payload_hook: crate::model::Hook = match serde_json::from_value(payload.clone()) {
            Ok(h) => h,
            Err(e) => {
                warn(&progress, format!("warning: hooks/{src_slug} → {tgt_slug}: payload not a valid Hook ({e:#}); skipping. Did you forget to set tgt overlay for required fields?"));
                skipped += 1;
                continue;
            }
        };
        let (payload_json_full, payload_code) = crate::snapshot::hook::serialize_hook(&payload_hook)?;
        // Drift check.
        let remote_hook = tgt_client.get_hook(tgt_id, None).await
            .with_context(|| format!("fetching tgt hook {tgt_id} for drift check"))?;
        let (remote_json_full, remote_code) = crate::snapshot::hook::serialize_hook(&remote_hook)?;
        let stripped = maybe_strip_overlay(remote_json_full.clone(), overlay_paths)?;
        let remote_combined_hash =
            crate::state::hook_combined_hash(&stripped, &remote_code);
        let in_sync = tgt_lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(tgt_slug))
            .and_then(|e| e.content_hash.as_deref())
            .map(|b| b == remote_combined_hash)
            .unwrap_or(true);
        if !in_sync {
            adopt_tgt_drift(&progress, tgt_lockfile, "hooks", tgt_slug, remote_combined_hash);
        }
        // Idempotency: compare canonical (env-specific fields stripped) JSON
        // plus the extracted code.
        if bytes_equal_after_strip(&payload_json_full, &remote_json_full, "hooks")?
            && payload_code == remote_code
        {
            continue;
        }
        if dry_run && diff {
            print_update_diff(
                &format!("hooks/{tgt_slug}.json"),
                &payload_json_full,
                &remote_json_full,
            );
            if payload_code != remote_code {
                print_update_diff(
                    &format!("hooks/{tgt_slug}.py"),
                    payload_code.clone().unwrap_or_default().as_bytes(),
                    remote_code.clone().unwrap_or_default().as_bytes(),
                );
            }
        }
        // PATCH (skipped in dry-run; counter still ticks).
        if !dry_run {
            tgt_client.update_hook(tgt_id, &payload_hook, None).await
                .with_context(|| format!("PATCH tgt hooks/{tgt_id} (mapped from src '{src_slug}')"))?;
        }
        applied.hooks += 1;
        progress.as_ref().map(|p| p.tick(tgt_slug));
    }

    // Rules ------------------------------------------------------------
    // Rules are a combined-hash kind (json + trigger_condition .py),
    // so the drift check and idempotency check both consider the
    // extracted code, not just the JSON bytes.
    if !mapping.rules.is_empty() {
        progress.as_ref().map(|p| {
            p.inc_total(mapping.rules.len() as u64);
            p.start_phase("update rules");
        });
    }
    let mut remote_rules_cache: Option<Vec<crate::model::Rule>> = None;
    for (src_slug, tgt_slug) in &mapping.rules {
        let Some(tgt_id) = lookup_tgt_id_w(&tgt_lockfile, "rules", tgt_slug, &mut skipped, &progress) else { continue };
        let mut payload = match crate::snapshot::rule::read_rule_value(&src_paths.rules_dir(), src_slug) {
            Ok(v) => v,
            Err(e) => { warn(&progress, format!("warning: cannot read src rules/{src_slug}: {e:#}")); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping, &empty_subs);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.rule(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_rule: crate::model::Rule = match serde_json::from_value(payload) {
            Ok(v) => v,
            Err(e) => { warn(&progress, format!("warning: rules/{src_slug} → {tgt_slug}: payload not a valid Rule ({e:#}); skipping")); skipped += 1; continue; }
        };
        let (payload_json_full, payload_code) = crate::snapshot::rule::serialize_rule(&payload_rule)?;

        if remote_rules_cache.is_none() {
            remote_rules_cache = Some(tgt_client.list_rules(None).await.context("listing tgt rules for drift check")?);
        }
        let cache = remote_rules_cache.as_ref().unwrap();
        let Some(remote) = cache.iter().find(|r| r.id == tgt_id) else {
            warn(&progress, format!("warning: rule id {tgt_id} not found on tgt remote; skipping"));
            skipped += 1;
            continue;
        };
        let (remote_json_full, remote_code) = crate::snapshot::rule::serialize_rule(remote)?;
        let stripped = maybe_strip_overlay(remote_json_full.clone(), overlay_paths)?;
        let remote_combined_hash =
            crate::state::rule_combined_hash(&stripped, &remote_code);
        let in_sync = tgt_lockfile
            .objects
            .get("rules")
            .and_then(|m| m.get(tgt_slug))
            .and_then(|e| e.content_hash.as_deref())
            .map(|b| b == remote_combined_hash)
            .unwrap_or(true);
        if !in_sync {
            adopt_tgt_drift(&progress, tgt_lockfile, "rules", tgt_slug, remote_combined_hash);
        }
        // Idempotency: both JSON and code must match (after env-specific strip).
        if bytes_equal_after_strip(&payload_json_full, &remote_json_full, "rules")?
            && payload_code == remote_code
        {
            continue;
        }
        if dry_run && diff {
            print_update_diff(
                &format!("rules/{tgt_slug}.json"),
                &payload_json_full,
                &remote_json_full,
            );
            if payload_code != remote_code {
                print_update_diff(
                    &format!("rules/{tgt_slug}.py"),
                    payload_code.clone().unwrap_or_default().as_bytes(),
                    remote_code.clone().unwrap_or_default().as_bytes(),
                );
            }
        }
        if !dry_run {
            tgt_client.update_rule(tgt_id, &payload_rule, None).await
                .with_context(|| format!("PATCH tgt rules/{tgt_id}"))?;
        }
        applied.rules += 1;
        progress.as_ref().map(|p| p.tick(tgt_slug));
    }

    // Labels -----------------------------------------------------------
    if !mapping.labels.is_empty() {
        progress.as_ref().map(|p| {
            p.inc_total(mapping.labels.len() as u64);
            p.start_phase("update labels");
        });
    }
    let mut remote_labels_cache: Option<Vec<crate::model::Label>> = None;
    for (src_slug, tgt_slug) in &mapping.labels {
        let Some(tgt_id) = lookup_tgt_id_w(&tgt_lockfile, "labels", tgt_slug, &mut skipped, &progress) else { continue };
        let path = src_paths.labels_dir().join(format!("{src_slug}.json"));
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => { warn(&progress, format!("warning: cannot read src labels/{src_slug}: {e:#}")); skipped += 1; continue; }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => { warn(&progress, format!("warning: parsing labels/{src_slug}: {e:#}")); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping, &empty_subs);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.label(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_label: crate::model::Label = match serde_json::from_value(payload) {
            Ok(v) => v,
            Err(e) => { warn(&progress, format!("warning: labels/{src_slug} → {tgt_slug}: payload not a valid Label ({e:#}); skipping")); skipped += 1; continue; }
        };
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_label).context("serializing payload label")?;
        payload_bytes.push(b'\n');
        if remote_labels_cache.is_none() {
            remote_labels_cache = Some(tgt_client.list_labels(None).await.context("listing tgt labels for drift check")?);
        }
        let cache = remote_labels_cache.as_ref().unwrap();
        let Some(remote) = cache.iter().find(|l| l.id == tgt_id) else {
            warn(&progress, format!("warning: label id {tgt_id} not found on tgt remote; skipping"));
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote).context("serializing remote label")?;
        remote_bytes.push(b'\n');
        let (in_sync, remote_hash) =
            tgt_drift_status(remote_bytes.clone(), overlay_paths, tgt_lockfile, "labels", tgt_slug)?;
        if !in_sync {
            adopt_tgt_drift(&progress, tgt_lockfile, "labels", tgt_slug, remote_hash);
        }
        if bytes_equal_after_strip(&payload_bytes, &remote_bytes, "labels")? {
            continue;
        }
        if dry_run && diff {
            print_update_diff(&format!("labels/{tgt_slug}.json"), &payload_bytes, &remote_bytes);
        }
        if !dry_run {
            tgt_client.update_label(tgt_id, &payload_label, None).await
                .with_context(|| format!("PATCH tgt labels/{tgt_id}"))?;
        }
        applied.labels += 1;
        progress.as_ref().map(|p| p.tick(tgt_slug));
    }

    // Queues -----------------------------------------------------------
    if !mapping.queues.is_empty() {
        progress.as_ref().map(|p| {
            p.inc_total(mapping.queues.len() as u64);
            p.start_phase("update queues");
        });
    }
    let mut remote_queues_cache: Option<Vec<crate::model::Queue>> = None;
    for (src_slug, tgt_slug) in &mapping.queues {
        let Some(tgt_id) = lookup_tgt_id_w(&tgt_lockfile, "queues", tgt_slug, &mut skipped, &progress) else { continue };
        let Some(src_queue_dir) = locate_queue_dir(&src_paths, src_slug) else {
            warn(&progress, format!("warning: cannot locate src queue '{src_slug}' on disk — skipping"));
            skipped += 1;
            continue;
        };
        let queue_path = src_queue_dir.join("queue.json");
        let raw = match std::fs::read_to_string(&queue_path) {
            Ok(r) => r,
            Err(e) => { warn(&progress, format!("warning: cannot read src queues/{src_slug}: {e:#}")); skipped += 1; continue; }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => { warn(&progress, format!("warning: parsing queue '{src_slug}': {e:#}")); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping, &empty_subs);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.queue(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        // Validate src payload parses as Queue (catch schema mismatches early)
        // but keep the rewritten Value for the actual PATCH so we can strip
        // server-computed sub-collections (`hooks`, `webhooks`, `rules`,
        // `inbox`, `counts`) that the Rossum API rejects on PATCH.
        if let Err(e) = serde_json::from_value::<crate::model::Queue>(payload.clone()) {
            warn(&progress, format!("warning: queues/{src_slug} → {tgt_slug}: payload not a valid Queue ({e:#}); skipping"));
            skipped += 1;
            continue;
        }
        let mut payload_for_patch = payload;
        strip_for_cross_env_patch(&mut payload_for_patch, "queues");
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_for_patch).context("serializing payload queue")?;
        payload_bytes.push(b'\n');
        if remote_queues_cache.is_none() {
            remote_queues_cache = Some(tgt_client.list_queues(None).await.context("listing tgt queues for drift check")?);
        }
        let cache = remote_queues_cache.as_ref().unwrap();
        let Some(remote) = cache.iter().find(|q| q.id == tgt_id) else {
            warn(&progress, format!("warning: queue id {tgt_id} not found on tgt remote; skipping"));
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote).context("serializing remote queue")?;
        remote_bytes.push(b'\n');
        let (in_sync, remote_hash) =
            tgt_drift_status(remote_bytes.clone(), overlay_paths, tgt_lockfile, "queues", tgt_slug)?;
        if !in_sync {
            adopt_tgt_drift(&progress, tgt_lockfile, "queues", tgt_slug, remote_hash);
        }
        if bytes_equal_after_strip(&payload_bytes, &remote_bytes, "queues")? {
            continue;
        }
        if dry_run && diff {
            print_update_diff(&format!("queues/{tgt_slug}.json"), &payload_bytes, &remote_bytes);
        }
        if !dry_run {
            tgt_client.patch_value(&format!("/queues/{tgt_id}"), &payload_for_patch, None).await
                .with_context(|| format!("PATCH tgt queues/{tgt_id}"))?;
        }
        applied.queues += 1;
        progress.as_ref().map(|p| p.tick(tgt_slug));
    }

    // Schemas ----------------------------------------------------------
    if !mapping.schemas.is_empty() {
        progress.as_ref().map(|p| {
            p.inc_total(mapping.schemas.len() as u64);
            p.start_phase("update schemas");
        });
    }
    for (src_slug, tgt_slug) in &mapping.schemas {
        let Some(tgt_id) = lookup_tgt_id_w(&tgt_lockfile, "schemas", tgt_slug, &mut skipped, &progress) else { continue };
        let Some(src_queue_dir) = locate_queue_dir(&src_paths, src_slug) else {
            warn(&progress, format!("warning: cannot locate src queue '{src_slug}' for schema — skipping"));
            skipped += 1;
            continue;
        };
        let mut payload = match read_schema_value(&src_queue_dir) {
            Ok(v) => v,
            Err(e) => { warn(&progress, format!("warning: cannot read src schema for queue '{src_slug}': {e:#}")); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping, &empty_subs);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.schema(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_schema: crate::model::Schema = match serde_json::from_value(payload) {
            Ok(s) => s,
            Err(e) => { warn(&progress, format!("warning: schemas/{src_slug} → {tgt_slug}: payload not a valid Schema ({e:#}); skipping")); skipped += 1; continue; }
        };
        let (payload_json_full, payload_formulas) =
            crate::snapshot::schema::serialize_schema(&payload_schema)?;
        let remote_schema = tgt_client.get_schema(tgt_id, None).await
            .with_context(|| format!("fetching tgt schema {tgt_id} for drift check"))?;
        let (remote_json_full, remote_formulas) =
            crate::snapshot::schema::serialize_schema(&remote_schema)?;
        let stripped = maybe_strip_overlay(remote_json_full.clone(), overlay_paths)?;
        let remote_combined_hash =
            crate::state::schema_combined_hash(&stripped, &remote_formulas);
        let in_sync = tgt_lockfile
            .objects
            .get("schemas")
            .and_then(|m| m.get(tgt_slug))
            .and_then(|e| e.content_hash.as_deref())
            .map(|b| b == remote_combined_hash)
            .unwrap_or(true);
        if !in_sync {
            adopt_tgt_drift(&progress, tgt_lockfile, "schemas", tgt_slug, remote_combined_hash);
        }
        if bytes_equal_after_strip(&payload_json_full, &remote_json_full, "schemas")?
            && payload_formulas == remote_formulas
        {
            continue;
        }
        if dry_run && diff {
            print_update_diff(
                &format!("schemas/{tgt_slug}/schema.json"),
                &payload_json_full,
                &remote_json_full,
            );
            // Diff each formula sidecar that differs. Set-of-formulas
            // mismatch (one side has a field the other doesn't) shows
            // as new-file or deleted-file diff.
            let local_map: std::collections::BTreeMap<&str, &[u8]> =
                payload_formulas.iter().map(|(k, v)| (k.as_str(), v.as_slice())).collect();
            let remote_map: std::collections::BTreeMap<&str, &[u8]> =
                remote_formulas.iter().map(|(k, v)| (k.as_str(), v.as_slice())).collect();
            let mut keys: std::collections::BTreeSet<&str> = local_map.keys().copied().collect();
            keys.extend(remote_map.keys().copied());
            for k in keys {
                let l = local_map.get(k).copied().unwrap_or(&[]);
                let r = remote_map.get(k).copied().unwrap_or(&[]);
                if l != r {
                    print_update_diff(
                        &format!("schemas/{tgt_slug}/formulas/{k}.py"),
                        l,
                        r,
                    );
                }
            }
        }
        if !dry_run {
            tgt_client.update_schema(tgt_id, &payload_schema, None).await
                .with_context(|| format!("PATCH tgt schemas/{tgt_id}"))?;
        }
        applied.schemas += 1;
        progress.as_ref().map(|p| p.tick(tgt_slug));
    }

    // Inboxes ----------------------------------------------------------
    if !mapping.inboxes.is_empty() {
        progress.as_ref().map(|p| {
            p.inc_total(mapping.inboxes.len() as u64);
            p.start_phase("update inboxes");
        });
    }
    for (src_slug, tgt_slug) in &mapping.inboxes {
        let Some(tgt_id) = lookup_tgt_id_w(&tgt_lockfile, "inboxes", tgt_slug, &mut skipped, &progress) else { continue };
        let Some(src_queue_dir) = locate_queue_dir(&src_paths, src_slug) else {
            warn(&progress, format!("warning: cannot locate src queue '{src_slug}' for inbox — skipping"));
            skipped += 1;
            continue;
        };
        let inbox_path = src_queue_dir.join("inbox.json");
        let raw = match std::fs::read_to_string(&inbox_path) {
            Ok(r) => r,
            Err(e) => { warn(&progress, format!("warning: cannot read src inbox for queue '{src_slug}': {e:#}")); skipped += 1; continue; }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => { warn(&progress, format!("warning: parsing inbox for queue '{src_slug}': {e:#}")); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping, &empty_subs);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.inbox(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_inbox: crate::model::Inbox = match serde_json::from_value(payload) {
            Ok(i) => i,
            Err(e) => { warn(&progress, format!("warning: inboxes/{src_slug} → {tgt_slug}: payload not a valid Inbox ({e:#}); skipping")); skipped += 1; continue; }
        };
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_inbox).context("serializing payload inbox")?;
        payload_bytes.push(b'\n');
        let remote_inbox = tgt_client.get_inbox(tgt_id, None).await
            .with_context(|| format!("fetching tgt inbox {tgt_id} for drift check"))?;
        let mut remote_bytes = serde_json::to_vec_pretty(&remote_inbox).context("serializing remote inbox")?;
        remote_bytes.push(b'\n');
        let (in_sync, remote_hash) =
            tgt_drift_status(remote_bytes.clone(), overlay_paths, tgt_lockfile, "inboxes", tgt_slug)?;
        if !in_sync {
            adopt_tgt_drift(&progress, tgt_lockfile, "inboxes", tgt_slug, remote_hash);
        }
        if bytes_equal_after_strip(&payload_bytes, &remote_bytes, "inboxes")? {
            continue;
        }
        if dry_run && diff {
            print_update_diff(&format!("inboxes/{tgt_slug}.json"), &payload_bytes, &remote_bytes);
        }
        if !dry_run {
            tgt_client.update_inbox(tgt_id, &payload_inbox, None).await
                .with_context(|| format!("PATCH tgt inboxes/{tgt_id}"))?;
        }
        applied.inboxes += 1;
        progress.as_ref().map(|p| p.tick(tgt_slug));
    }

    // Email templates --------------------------------------------------
    if !mapping.email_templates.is_empty() {
        progress.as_ref().map(|p| {
            p.inc_total(mapping.email_templates.len() as u64);
            p.start_phase("update email_templates");
        });
    }
    let mut remote_template_cache: Option<Vec<crate::model::EmailTemplate>> = None;
    for (src_key, tgt_key) in &mapping.email_templates {
        let Some(tgt_id) = lookup_tgt_id_w(&tgt_lockfile, "email_templates", tgt_key, &mut skipped, &progress) else { continue };
        let Some((ws, q, t)) = split_template_key(src_key) else {
            warn(&progress, format!("warning: src email_template key '{src_key}' is not <ws>/<q>/<template>; skipping"));
            skipped += 1;
            continue;
        };
        let templates_dir = src_paths.queue_email_templates_dir(ws, q);
        let src_template = match read_email_template(&templates_dir, t) {
            Ok(t) => t,
            Err(e) => { warn(&progress, format!("warning: cannot read src email_template '{src_key}': {e:#}")); skipped += 1; continue; }
        };
        let mut payload = serde_json::to_value(&src_template).context("serializing src email template to value")?;
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping, &empty_subs);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.email_template(tgt_key));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        // Validate as EmailTemplate (catch schema mismatches), but keep the
        // Value for the PATCH so we can strip `triggers` — which references a
        // non-deployable sub-resource and 400s the API on cross-env send.
        if let Err(e) = serde_json::from_value::<crate::model::EmailTemplate>(payload.clone()) {
            warn(&progress, format!("warning: email_templates/{src_key} → {tgt_key}: payload not a valid EmailTemplate ({e:#}); skipping"));
            skipped += 1;
            continue;
        }
        let mut payload_for_patch = payload;
        strip_for_cross_env_patch(&mut payload_for_patch, "email_templates");
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_for_patch).context("serializing payload email template")?;
        payload_bytes.push(b'\n');
        if remote_template_cache.is_none() {
            remote_template_cache = Some(tgt_client.list_email_templates(None).await
                .context("listing tgt email templates for drift check")?);
        }
        let cache = remote_template_cache.as_ref().unwrap();
        let Some(remote_template) = cache.iter().find(|t| t.id == tgt_id) else {
            warn(&progress, format!("warning: email_template id {tgt_id} not found on tgt remote; skipping"));
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote_template).context("serializing remote email template")?;
        remote_bytes.push(b'\n');
        let (in_sync, remote_hash) =
            tgt_drift_status(remote_bytes.clone(), overlay_paths, tgt_lockfile, "email_templates", tgt_key)?;
        if !in_sync {
            adopt_tgt_drift(&progress, tgt_lockfile, "email_templates", tgt_key, remote_hash);
        }
        if bytes_equal_after_strip(&payload_bytes, &remote_bytes, "email_templates")? {
            continue;
        }
        if dry_run && diff {
            print_update_diff(
                &format!("email_templates/{tgt_key}.json"),
                &payload_bytes,
                &remote_bytes,
            );
        }
        if !dry_run {
            tgt_client.patch_value(&format!("/email_templates/{tgt_id}"), &payload_for_patch, None).await
                .with_context(|| format!("PATCH tgt email_templates/{tgt_id}"))?;
        }
        applied.email_templates += 1;
        progress.as_ref().map(|p| p.tick(tgt_key));
    }

    // Engines ----------------------------------------------------------
    if !mapping.engines.is_empty() {
        progress.as_ref().map(|p| {
            p.inc_total(mapping.engines.len() as u64);
            p.start_phase("update engines");
        });
    }
    for (src_slug, tgt_slug) in &mapping.engines {
        let Some(tgt_id) = lookup_tgt_id_w(&tgt_lockfile, "engines", tgt_slug, &mut skipped, &progress) else { continue };
        let path = src_paths.engines_dir().join(format!("{src_slug}.json"));
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => { warn(&progress, format!("warning: cannot read src engines/{src_slug}: {e:#}")); skipped += 1; continue; }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => { warn(&progress, format!("warning: parsing engines/{src_slug}: {e:#}")); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping, &empty_subs);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.engine(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_engine: crate::model::Engine = match serde_json::from_value(payload) {
            Ok(e) => e,
            Err(e) => { warn(&progress, format!("warning: engines/{src_slug} → {tgt_slug}: payload not a valid Engine ({e:#}); skipping")); skipped += 1; continue; }
        };
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_engine).context("serializing payload engine")?;
        payload_bytes.push(b'\n');
        let remotes = tgt_client.list_engines(None).await.context("listing tgt engines for drift check")?;
        let Some(remote) = remotes.iter().find(|e| e.id == tgt_id) else {
            warn(&progress, format!("warning: engine id {tgt_id} not found on tgt remote; skipping"));
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote).context("serializing remote engine")?;
        remote_bytes.push(b'\n');
        let (in_sync, remote_hash) =
            tgt_drift_status(remote_bytes.clone(), overlay_paths, tgt_lockfile, "engines", tgt_slug)?;
        if !in_sync {
            adopt_tgt_drift(&progress, tgt_lockfile, "engines", tgt_slug, remote_hash);
        }
        if bytes_equal_after_strip(&payload_bytes, &remote_bytes, "engines")? {
            continue;
        }
        if dry_run && diff {
            print_update_diff(&format!("engines/{tgt_slug}.json"), &payload_bytes, &remote_bytes);
        }
        if dry_run {
            applied.engines += 1;
            progress.as_ref().map(|p| p.tick(tgt_slug));
        } else {
            match tgt_client.update_engine(tgt_id, &payload_engine, None).await
                .with_context(|| format!("PATCH tgt engines/{tgt_id}"))
            {
                Ok(_) => {
                    applied.engines += 1;
                    progress.as_ref().map(|p| p.tick(tgt_slug));
                }
                Err(e) if anyhow_has_status(&e, 405) => {
                    warn(&progress, format!("warning: engines are not writable via PATCH on tgt org/plan (405). Skipping all engine apply."));
                    skipped += 1;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
    }

    // Engine fields ----------------------------------------------------
    if !mapping.engine_fields.is_empty() {
        progress.as_ref().map(|p| {
            p.inc_total(mapping.engine_fields.len() as u64);
            p.start_phase("update engine_fields");
        });
    }
    for (src_slug, tgt_slug) in &mapping.engine_fields {
        let Some(tgt_id) = lookup_tgt_id_w(&tgt_lockfile, "engine_fields", tgt_slug, &mut skipped, &progress) else { continue };
        let Some(path) = locate_engine_field_path(&src_paths, src_slug) else {
            warn(&progress, format!(
                "warning: cannot locate src engine field '{src_slug}' under any engine dir; skipping"
            ));
            skipped += 1;
            continue;
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => { warn(&progress, format!("warning: cannot read src engine field '{src_slug}': {e:#}")); skipped += 1; continue; }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => { warn(&progress, format!("warning: parsing engine-fields/{src_slug}: {e:#}")); skipped += 1; continue; }
        };
        rewrite_urls(&mut payload, &src_lockfile, &tgt_lockfile, &mapping, &empty_subs);
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.engine_field(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_field: crate::model::EngineField = match serde_json::from_value(payload) {
            Ok(f) => f,
            Err(e) => { warn(&progress, format!("warning: engine-fields/{src_slug} → {tgt_slug}: payload not a valid EngineField ({e:#}); skipping")); skipped += 1; continue; }
        };
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_field).context("serializing payload engine field")?;
        payload_bytes.push(b'\n');
        let remotes = tgt_client.list_engine_fields(None).await.context("listing tgt engine fields for drift check")?;
        let Some(remote) = remotes.iter().find(|f| f.id == tgt_id) else {
            warn(&progress, format!("warning: engine_field id {tgt_id} not found on tgt remote; skipping"));
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote).context("serializing remote engine field")?;
        remote_bytes.push(b'\n');
        let (in_sync, remote_hash) =
            tgt_drift_status(remote_bytes.clone(), overlay_paths, tgt_lockfile, "engine_fields", tgt_slug)?;
        if !in_sync {
            adopt_tgt_drift(&progress, tgt_lockfile, "engine_fields", tgt_slug, remote_hash);
        }
        if bytes_equal_after_strip(&payload_bytes, &remote_bytes, "engine_fields")? {
            continue;
        }
        if dry_run && diff {
            print_update_diff(
                &format!("engine_fields/{tgt_slug}.json"),
                &payload_bytes,
                &remote_bytes,
            );
        }
        if dry_run {
            applied.engine_fields += 1;
            progress.as_ref().map(|p| p.tick(tgt_slug));
        } else {
            match tgt_client.update_engine_field(tgt_id, &payload_field, None).await
                .with_context(|| format!("PATCH tgt engine_fields/{tgt_id}"))
            {
                Ok(_) => {
                    applied.engine_fields += 1;
                    progress.as_ref().map(|p| p.tick(tgt_slug));
                }
                Err(e) if anyhow_has_status(&e, 405) => {
                    warn(&progress, format!("warning: engine fields are not writable via PATCH on tgt org/plan (405). Skipping all engine field apply."));
                    skipped += 1;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
    }

    let total = applied.total();
    let verb = if dry_run { "Would apply" } else { "Applied" };
    let suffix = if dry_run { "(dry run, no PATCHes sent)" } else { "PATCHes" };
    let mut summary = if dry_run {
        format!(
            "{verb} {} hooks, {} rules, {} labels, {} queues, {} schemas, {} inboxes, \
{} email templates, {} engines, {} engine fields ({} change(s)) from {src} to {tgt} {suffix}",
            applied.hooks, applied.rules, applied.labels, applied.queues,
            applied.schemas, applied.inboxes, applied.email_templates,
            applied.engines, applied.engine_fields, total,
        )
    } else {
        format!(
            "Applied {} hooks, {} rules, {} labels, {} queues, {} schemas, {} inboxes, \
{} email templates, {} engines, {} engine fields ({} PATCHes) from {src} to {tgt}",
            applied.hooks, applied.rules, applied.labels, applied.queues,
            applied.schemas, applied.inboxes, applied.email_templates,
            applied.engines, applied.engine_fields, total,
        )
    };
    if skipped > 0 {
        summary.push_str(&format!(", {skipped} skipped"));
    }
    Ok(summary)
}

/// Emit a `--- src / +++ tgt remote` unified diff for one update.
/// Skipped silently when bytes are equal (matches `print_unified`).
fn print_update_diff(label: &str, src: &[u8], tgt_remote: &[u8]) {
    let l = String::from_utf8_lossy(src);
    let r = String::from_utf8_lossy(tgt_remote);
    crate::cli::diff::print_unified(
        &format!("{label} (src after overlay+rewrite)"),
        &format!("{label} (tgt remote)"),
        &l,
        &r,
        &mut 0,
    );
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

fn lookup_tgt_id_w(
    tgt_lockfile: &Lockfile,
    kind: &str,
    tgt_slug: &str,
    skipped: &mut usize,
    progress: &Option<Arc<OverallProgress>>,
) -> Option<u64> {
    match tgt_lockfile.objects.get(kind).and_then(|m| m.get(tgt_slug)).map(|e| e.id) {
        Some(id) => Some(id),
        None => {
            warn(progress, format!(
                "warning: tgt lockfile has no entry for {kind}/{tgt_slug} — skipping (run `rdc pull <tgt>` first)"
            ));
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
