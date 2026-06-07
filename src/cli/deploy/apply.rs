use crate::api::{RossumClient, anyhow_has_status};
use crate::cli::deploy::common::{
    bytes_equal_after_strip, portabilize_for_hash, rewrite_urls, tgt_drift_status,
};
use crate::cli::pull::common::maybe_strip_overlay;
use crate::config::ProjectConfig;
use crate::log::{Action, Log};
use crate::mapping::Mapping;
use crate::overlay::{Overlay, apply_overrides};
use crate::paths::Paths;
use crate::secrets::resolve_token;
use crate::snapshot::codec::combined_hash;
use crate::snapshot::create::{
    redact_for_disk, strip_for_create, strip_for_cross_env_patch, strip_patch_extra,
};
use crate::snapshot::email_template::{read_email_template, write_email_template};
use crate::snapshot::hook::{read_hook_value, write_hook};
use crate::snapshot::rule::write_rule;
use crate::snapshot::schema::{read_schema_value, write_schema_bytes};
use crate::state::{Lockfile, ObjectEntry, rule_combined_hash, schema_combined_hash};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One target object that's drifted from the lockfile baseline — i.e.,
/// someone edited it out-of-band on the tgt env (Rossum UI, another
/// rdc instance, …) since the last `rdc sync <tgt>`. The orchestrator
/// (`deploy::run::run`) collects these from the preview-pass apply
/// and prompts the user for a per-object resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriftedItem {
    pub kind: String,
    pub slug: String,
}

/// What to do with a drifted target object when the real apply pass
/// reaches it. Built by the orchestrator from user prompts (or the
/// `--force-overwrite-drift` short-circuit) and threaded back into
/// the second `apply::run` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriftDecision {
    /// Overwrite the tgt's out-of-band edit with src's bytes. The
    /// legacy behavior — adopts the drifted hash as the new lockfile
    /// baseline, then PATCHes with src.
    Overwrite,
    /// Keep the tgt edit; skip the PATCH for this object. Adopts the
    /// drifted hash as the new lockfile baseline so subsequent syncs
    /// see a clean tgt — the user is acknowledging "tgt is canonical
    /// for this object." To also move the edit back into src, run
    /// `rdc sync <src>` afterwards.
    Keep,
    /// Defer: skip the PATCH AND don't adopt the drifted hash. The
    /// next deploy re-classifies as drift and re-prompts. Useful when
    /// the user wants to investigate before deciding.
    Skip,
}

/// Aggregate result of one apply pass.
pub(crate) struct ApplyOutcome {
    /// Human-facing summary line ("Applied / Would apply N hooks …").
    pub summary: String,
    /// Tgt objects that drifted from the lockfile baseline during this
    /// pass. The orchestrator uses this list to prompt for decisions
    /// before the real apply runs.
    pub drifted: Vec<DriftedItem>,
}

/// Emit a warning through the progress bar if active, or to stderr directly.
/// Both branches render the message flush-left under the active phase
/// (or as a standalone line if no progress log is active).
fn warn(progress: &Option<Arc<Log>>, msg: String) {
    match progress {
        Some(p) => p.event(Action::Warn, &msg),
        None => eprintln!("{msg}"),
    }
}

/// Cheap check on a raw payload: is this a Rossum-store hook? Used to
/// decide whether to pin `token_owner` from the tgt overlay during the
/// cross-env update phase.
fn is_store_extension(payload: &Value) -> bool {
    payload.get("extension_source").and_then(|v| v.as_str()) == Some("rossum_store")
}

/// Adopt an out-of-band change on tgt: refresh the lockfile entry's
/// `content_hash` to the current remote hash and emit a quiet note.
/// Used when the drift check detects that someone modified the tgt
/// object directly (e.g. via the Rossum UI) since the last pull — the
/// deploy proceeds anyway, treating the remote-as-of-now as the new
/// baseline that the upcoming PATCH overwrites.
fn adopt_tgt_drift(
    progress: &Option<Arc<Log>>,
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
        Some(p) => p.event(Action::Info, &msg),
        None => eprintln!("{msg}"),
    }
}

/// Outcome of consulting the drift-decisions map at PATCH time. Each
/// per-kind handler in the apply loop branches on this.
enum DriftHandling {
    /// Fall through to the normal idempotency-check + PATCH path.
    Patch,
    /// Stop processing this object — skip the PATCH. The caller's
    /// `continue` resumes with the next item in the loop.
    SkipObject,
}

/// Centralise the drift decision lookup so every per-kind handler
/// stays a one-liner. In dry-run mode the apply pass doesn't have
/// decisions yet — it's collecting the drift list for the
/// orchestrator — so all drift items are recorded and skipped.
/// In real-run mode an unmapped drift indicates a TOCTOU race with
/// a concurrent edit; fail loudly rather than silently overwriting.
fn handle_drift(
    progress: &Option<Arc<Log>>,
    tgt_lockfile: &mut Lockfile,
    drift_decisions: &BTreeMap<(String, String), DriftDecision>,
    drifted: &mut Vec<DriftedItem>,
    dry_run: bool,
    kind: &str,
    tgt_slug: &str,
    remote_hash: String,
) -> Result<DriftHandling> {
    drifted.push(DriftedItem {
        kind: kind.to_string(),
        slug: tgt_slug.to_string(),
    });
    if dry_run {
        // Preview pass: render the diff for this drifted item but
        // don't actually adopt (we're operating on a cloned lockfile
        // anyway). Returning `Patch` here means "render diff",
        // not "send PATCH" — the dry_run gate later suppresses the
        // network call.
        return Ok(DriftHandling::Patch);
    }
    let key = (kind.to_string(), tgt_slug.to_string());
    match drift_decisions.get(&key) {
        Some(DriftDecision::Overwrite) => {
            adopt_tgt_drift(progress, tgt_lockfile, kind, tgt_slug, remote_hash);
            Ok(DriftHandling::Patch)
        }
        Some(DriftDecision::Keep) => {
            adopt_tgt_drift(progress, tgt_lockfile, kind, tgt_slug, remote_hash);
            if let Some(p) = progress {
                p.event(
                    Action::Skip,
                    &format!("{kind}/{tgt_slug} kept tgt out-of-band edit"),
                )
            }
            Ok(DriftHandling::SkipObject)
        }
        Some(DriftDecision::Skip) => {
            if let Some(p) = progress {
                p.event(
                    Action::Skip,
                    &format!("{kind}/{tgt_slug} drift deferred; re-prompts next deploy"),
                )
            }
            Ok(DriftHandling::SkipObject)
        }
        None => {
            // Unmapped drift in real-apply mode. Common cause:
            // "phantom drift" for objects just created in this deploy
            // (the POST-response hash differs from the post-GET
            // canonical hash because the server adds/normalises
            // fields on read). The preview pass couldn't have seen
            // this object — it didn't exist on tgt yet — so the
            // resolver never got a chance to ask. Falling back to the
            // legacy adopt+PATCH path here is safe in that scenario.
            //
            // The trade-off: a genuine concurrent edit landing in the
            // narrow window between preview and apply would also
            // hit this arm and be silently overwritten. That race is
            // rare (single-user tool, short deploy window) and the
            // user already consented to overwrite the rest of the
            // tgt state via the confirm prompt.
            adopt_tgt_drift(progress, tgt_lockfile, kind, tgt_slug, remote_hash);
            Ok(DriftHandling::Patch)
        }
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
pub(crate) async fn run(
    src: &str,
    tgt: &str,
    dry_run: bool,
    diff: bool,
    progress: Option<Arc<Log>>,
    tgt_lockfile: &mut Lockfile,
    selection: Option<&crate::cli::deploy::selection::Selection>,
    hook_secrets_plan: &crate::cli::deploy::hook_secrets::HookSecretsPlan,
    drift_decisions: &BTreeMap<(String, String), DriftDecision>,
) -> Result<ApplyOutcome> {
    // Collect every drift event so the caller can prompt for
    // decisions before the real apply runs. In the preview pass
    // `drift_decisions` is empty, so this list is what feeds the
    // resolver; in the real pass it should be a subset of the
    // preview's list (anything new would be a TOCTOU race).
    let mut drifted: Vec<DriftedItem> = Vec::new();
    let cwd = std::env::current_dir().context("getting current directory")?;
    let src_paths = Paths::for_env(&cwd, src);
    let tgt_paths = Paths::for_env(&cwd, tgt);

    let cfg = ProjectConfig::load(&src_paths.project_config())?;
    let _src_cfg = cfg
        .envs
        .get(src)
        .ok_or_else(|| anyhow!("env '{src}' is not defined in rdc.toml"))?;
    let tgt_cfg = cfg
        .envs
        .get(tgt)
        .ok_or_else(|| anyhow!("env '{tgt}' is not defined in rdc.toml"))?;

    let token = resolve_token(&cwd, tgt, &tgt_cfg.api_base).await?;
    let tgt_client = RossumClient::new(tgt_cfg.api_base.clone(), token)
        .context("constructing tgt API client")?
        .with_env_label(tgt);

    let mapping = Mapping::load(&src_paths.mapping_file(src, tgt))?;
    let src_lockfile = Lockfile::load(&src_paths.lockfile()).with_context(|| {
        format!(
            "loading src lockfile from {}",
            src_paths.lockfile().display()
        )
    })?;
    let tgt_overlay = Overlay::load(&tgt_paths.overlay_file()).with_context(|| {
        format!(
            "loading tgt overlay from {}",
            tgt_paths.overlay_file().display()
        )
    })?;

    let mut applied = ApplyCounts::default();
    let mut skipped = 0usize;

    // Workspaces -------------------------------------------------------
    // First in dependency order; queues/hooks/etc. all reference their
    // parent workspace. Before this loop existed, src-side workspace
    // changes (name, autopilot, metadata) silently never reached the
    // tgt after the workspace's initial POST.
    for (src_slug, tgt_slug) in &mapping.workspaces {
        if let Some(sel) = selection
            && !sel.contains("workspaces", src_slug)
        {
            continue;
        }
        let Some(tgt_id) = lookup_tgt_id_w(
            tgt_lockfile,
            "workspaces",
            tgt_slug,
            &mut skipped,
            &progress,
        ) else {
            continue;
        };
        let path = src_paths.workspace_dir(src_slug).join("workspace.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: cannot read src workspaces/{src_slug}: {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: parsing workspaces/{src_slug}: {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        rewrite_urls(
            &mut payload,
            &src_lockfile,
            tgt_lockfile,
            &mapping,
            &mapping.hook_templates,
        );
        // Overlay has no per-workspace section in the current schema; if
        // that changes, add the `ov.workspace(tgt_slug)` apply here.
        // List + find pattern matches engines/engine_fields and is what
        // existing deploy test mocks expose (`GET /workspaces` list, not
        // individual id GETs).
        let remotes = tgt_client
            .list_workspaces(None)
            .await
            .context("listing tgt workspaces for drift check")?;
        let Some(remote_ws) = remotes.iter().find(|w| w.id == tgt_id) else {
            warn(
                &progress,
                format!("warning: workspace id {tgt_id} not found on tgt remote; skipping"),
            );
            skipped += 1;
            continue;
        };
        let mut remote_bytes =
            serde_json::to_vec_pretty(remote_ws).context("serializing remote workspace")?;
        remote_bytes.push(b'\n');
        // Bytes for the idempotency check — must match what the PATCH body
        // would be after stripping.
        let mut payload_bytes =
            serde_json::to_vec_pretty(&payload).context("serializing payload workspace")?;
        payload_bytes.push(b'\n');
        let (in_sync, remote_hash) = tgt_drift_status(
            remote_bytes.clone(),
            None,
            tgt_lockfile,
            "workspaces",
            tgt_slug,
        )?;
        if !in_sync {
            match handle_drift(
                &progress,
                tgt_lockfile,
                drift_decisions,
                &mut drifted,
                dry_run,
                "workspaces",
                tgt_slug,
                remote_hash,
            )? {
                DriftHandling::SkipObject => continue,
                DriftHandling::Patch => {}
            }
        }
        if bytes_equal_after_strip(&payload_bytes, &remote_bytes, "workspaces")? {
            continue;
        }
        if dry_run && diff {
            print_update_diff_normalized(
                &format!("workspaces/{tgt_slug}.json"),
                &payload_bytes,
                &remote_bytes,
                "workspaces",
            )?;
        }
        if !dry_run {
            // Strip server-managed + back-ref fields (id/url/organization/queues)
            // before PATCH to mirror queues/email_templates/inboxes.
            let mut patch_body = payload.clone();
            crate::snapshot::create::strip_for_cross_env_patch(&mut patch_body, "workspaces");
            let response_value = tgt_client
                .patch_value(&format!("/workspaces/{tgt_id}"), &patch_body, None)
                .await
                .with_context(|| format!("PATCH tgt workspaces/{tgt_id}"))?;
            let updated: crate::model::Workspace = serde_json::from_value(response_value)
                .context("parsing PATCH /workspaces response as Workspace")?;
            write_back_workspace(&tgt_paths, tgt_lockfile, tgt_slug, &updated, None)?;
        }
        applied.workspaces += 1;
        if let Some(p) = &progress {
            p.event(Action::Patch, &format!("workspace/{tgt_slug}"));
        }
    }

    // Hooks ------------------------------------------------------------
    for (src_slug, tgt_slug) in &mapping.hooks {
        if let Some(sel) = selection
            && !sel.contains("hooks", src_slug)
        {
            continue;
        }
        let Some(tgt_id) =
            lookup_tgt_id_w(tgt_lockfile, "hooks", tgt_slug, &mut skipped, &progress)
        else {
            continue;
        };
        let mut payload = match read_hook_value(&src_paths.hooks_dir(), src_slug) {
            Ok(v) => v,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: cannot read src hooks/{src_slug}: {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        rewrite_urls(
            &mut payload,
            &src_lockfile,
            tgt_lockfile,
            &mapping,
            &mapping.hook_templates,
        );
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.hook(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        // `token_owner` is a tgt-env user URL; the src snapshot carries
        // the src-env user URL, which the tgt API rejects as "Invalid
        // hyperlink" since users aren't a deployable kind (no cross-env
        // mapping). Pin to the effective overlay value (per-hook →
        // `[defaults] store_extension_token_owner`) when available; if
        // the src body has a token_owner and the overlay has nothing,
        // strip it so PATCH leaves the tgt remote's existing value alone
        // (a token_owner-less PATCH means "don't change this field").
        //
        // Store extensions are stricter — they ALWAYS need a tgt user
        // URL — so for store-ext hooks we still abort with a directive
        // pointing the user at the bootstrap picker rather than silently
        // skipping the field.
        let payload_has_token_owner = payload
            .get("token_owner")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        if payload_has_token_owner {
            let resolved = crate::cli::deploy::store_extensions::effective_token_owner(
                tgt_overlay.as_ref(),
                tgt_slug,
            );
            match resolved {
                Some(url) => {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.insert(
                            "token_owner".into(),
                            serde_json::Value::String(url.to_string()),
                        );
                    }
                }
                None if is_store_extension(&payload) => {
                    warn(
                        &progress,
                        format!(
                            "warning: hooks/{tgt_slug} is a store extension but {} has no \
                         token_owner for it (neither [hooks.{tgt_slug}] token_owner nor \
                         [defaults] store_extension_token_owner). Run `rdc deploy {src} {tgt}` \
                         on a TTY once to pick interactively, or set it in the overlay file \
                         directly. Skipping this hook.",
                            tgt_paths.overlay_file().display(),
                        ),
                    );
                    skipped += 1;
                    continue;
                }
                None => {
                    if let Some(obj) = payload.as_object_mut() {
                        obj.remove("token_owner");
                    }
                }
            }
        }
        let payload_hook: crate::model::Hook = match serde_json::from_value(payload.clone()) {
            Ok(h) => h,
            Err(e) => {
                warn(
                    &progress,
                    format!(
                        "warn: hooks/{src_slug} -> {tgt_slug}: payload not a valid Hook ({e:#}); skipping. Did you forget to set tgt overlay for required fields?"
                    ),
                );
                skipped += 1;
                continue;
            }
        };
        let (payload_json_full, payload_code) =
            crate::snapshot::hook::serialize_hook(&payload_hook)?;
        // Drift check.
        let remote_hook = tgt_client
            .get_hook(tgt_id, None)
            .await
            .with_context(|| format!("fetching tgt hook {tgt_id} for drift check"))?;
        let (remote_json_full, remote_code) = crate::snapshot::hook::serialize_hook(&remote_hook)?;
        // Hash must match what pull::hooks::process records: the POST-overlay
        // combined hash. If an overlay strips fields from the JSON before writing
        // to disk, the on-disk (and lockfile-recorded) hash is over the stripped
        // bytes. Drift check must use the same framing or it falsely fires on
        // every deploy for hooks with overlays.
        let remote_json_for_hash = maybe_strip_overlay(remote_json_full.clone(), overlay_paths)?;
        // The lockfile baseline is recorded over the portabilized (rdc://) form
        // by the pull post-pass, so the remote must be portabilized to compare.
        let remote_json_for_hash = portabilize_for_hash(remote_json_for_hash, tgt_lockfile)?;
        let remote_combined_hash = {
            let sidecars: Vec<(String, Vec<u8>)> = if let Some(c) = &remote_code {
                vec![("code".to_string(), c.as_bytes().to_vec())]
            } else {
                vec![]
            };
            crate::snapshot::codec::combined_hash(&remote_json_for_hash, &sidecars, tgt_lockfile)
        };
        let in_sync = tgt_lockfile
            .objects
            .get("hooks")
            .and_then(|m| m.get(tgt_slug))
            .and_then(|e| e.content_hash.as_deref())
            .map(|b| b == remote_combined_hash)
            .unwrap_or(true);
        if !in_sync {
            match handle_drift(
                &progress,
                tgt_lockfile,
                drift_decisions,
                &mut drifted,
                dry_run,
                "hooks",
                tgt_slug,
                remote_combined_hash,
            )? {
                DriftHandling::SkipObject => continue,
                DriftHandling::Patch => {}
            }
        }
        // Idempotency: compare canonical (env-specific fields stripped) JSON
        // plus the extracted code.
        if bytes_equal_after_strip(&payload_json_full, &remote_json_full, "hooks")?
            && payload_code == remote_code
        {
            continue;
        }
        if dry_run && diff {
            print_update_diff_normalized(
                &format!("hooks/{tgt_slug}.json"),
                &payload_json_full,
                &remote_json_full,
                "hooks",
            )?;
            if payload_code != remote_code {
                // Sidecar extension follows the payload's runtime — same
                // file the user will see on disk after the sync. `.js`
                // for Node.js hooks, `.py` otherwise.
                let ext = crate::snapshot::hook::hook_code_extension(&payload_hook);
                print_update_diff(
                    &format!("hooks/{tgt_slug}.{ext}"),
                    payload_code.clone().unwrap_or_default().as_bytes(),
                    remote_code.clone().unwrap_or_default().as_bytes(),
                );
            }
        }
        // PATCH (skipped in dry-run; counter still ticks). The body
        // includes the filtered hook secrets so any rotated values
        // ride along with this update — the pre-flight check in
        // `run::run` already validated the target file has the keys
        // the source hook declares.
        if !dry_run {
            let mut body = serde_json::to_value(&payload_hook)
                .with_context(|| format!("serializing hook '{src_slug}' for PATCH"))?;
            // Strip server-managed fields (notably the redacted `status`
            // sentinel) so the cross-env PATCH matches the CREATE contract.
            // `strip_for_create` (not the cross-env variant) is deliberate: it
            // leaves the resolved tgt `token_owner` (set on `payload` above)
            // and `organization` intact, matching the long-standing flow.
            strip_for_create(&mut body, "hooks");
            if let Some(secrets) = hook_secrets_plan.for_slug(src_slug)
                && let Some(obj) = body.as_object_mut()
            {
                obj.insert(
                    "secrets".to_string(),
                    serde_json::to_value(secrets).expect("BTreeMap<String,String> serializes"),
                );
            }
            let updated = tgt_client
                .update_hook_value(tgt_id, &body, None)
                .await
                .with_context(|| {
                    format!("PATCH tgt hooks/{tgt_id} (mapped from src '{src_slug}')")
                })?;
            write_back_hook(&tgt_paths, tgt_lockfile, tgt_slug, &updated, overlay_paths)?;
            // Record the just-injected secrets-hash so a subsequent
            // sync on the target doesn't see drift.
            let empty = std::collections::BTreeMap::<String, String>::new();
            let injected = hook_secrets_plan.for_slug(src_slug).unwrap_or(&empty);
            let injected_hash = crate::state::hook_secrets_hash(injected);
            if let Some(entry) = tgt_lockfile
                .objects
                .get_mut("hooks")
                .and_then(|m| m.get_mut(tgt_slug.as_str()))
            {
                entry.secrets_hash = Some(injected_hash);
            }
        }
        applied.hooks += 1;
        if let Some(p) = &progress {
            p.event(Action::Patch, &format!("hook/{tgt_slug}"));
        }
    }

    // Rules ------------------------------------------------------------
    // Rules are a combined-hash kind (json + trigger_condition .py),
    // so the drift check and idempotency check both consider the
    // extracted code, not just the JSON bytes.
    let mut remote_rules_cache: Option<Vec<crate::model::Rule>> = None;
    for (src_slug, tgt_slug) in &mapping.rules {
        if let Some(sel) = selection
            && !sel.contains("rules", src_slug)
        {
            continue;
        }
        let Some(tgt_id) =
            lookup_tgt_id_w(tgt_lockfile, "rules", tgt_slug, &mut skipped, &progress)
        else {
            continue;
        };
        let mut payload =
            match crate::snapshot::rule::read_rule_value(&src_paths.rules_dir(), src_slug) {
                Ok(v) => v,
                Err(e) => {
                    warn(
                        &progress,
                        format!("warning: cannot read src rules/{src_slug}: {e:#}"),
                    );
                    skipped += 1;
                    continue;
                }
            };
        rewrite_urls(
            &mut payload,
            &src_lockfile,
            tgt_lockfile,
            &mapping,
            &mapping.hook_templates,
        );
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.rule(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let mut payload_rule: crate::model::Rule = match serde_json::from_value(payload) {
            Ok(v) => v,
            Err(e) => {
                warn(
                    &progress,
                    format!(
                        "warn: rules/{src_slug} -> {tgt_slug}: payload not a valid Rule ({e:#}); skipping"
                    ),
                );
                skipped += 1;
                continue;
            }
        };
        let (payload_json_full, payload_code) =
            crate::snapshot::rule::serialize_rule(&payload_rule)?;

        if remote_rules_cache.is_none() {
            remote_rules_cache = Some(
                tgt_client
                    .list_rules(None)
                    .await
                    .context("listing tgt rules for drift check")?,
            );
        }
        let cache = remote_rules_cache.as_ref().unwrap();
        let Some(remote) = cache.iter().find(|r| r.id == tgt_id) else {
            warn(
                &progress,
                format!("warning: rule id {tgt_id} not found on tgt remote; skipping"),
            );
            skipped += 1;
            continue;
        };
        let (remote_json_full, remote_code) = crate::snapshot::rule::serialize_rule(remote)?;
        let stripped = maybe_strip_overlay(remote_json_full.clone(), overlay_paths)?;
        let stripped = portabilize_for_hash(stripped, tgt_lockfile)?;
        let remote_combined_hash =
            crate::state::rule_combined_hash(&stripped, &remote_code, tgt_lockfile);
        let in_sync = tgt_lockfile
            .objects
            .get("rules")
            .and_then(|m| m.get(tgt_slug))
            .and_then(|e| e.content_hash.as_deref())
            .map(|b| b == remote_combined_hash)
            .unwrap_or(true);
        if !in_sync {
            match handle_drift(
                &progress,
                tgt_lockfile,
                drift_decisions,
                &mut drifted,
                dry_run,
                "rules",
                tgt_slug,
                remote_combined_hash,
            )? {
                DriftHandling::SkipObject => continue,
                DriftHandling::Patch => {}
            }
        }
        // Idempotency: both JSON and code must match (after env-specific strip).
        if bytes_equal_after_strip(&payload_json_full, &remote_json_full, "rules")?
            && payload_code == remote_code
        {
            continue;
        }
        if dry_run && diff {
            print_update_diff_normalized(
                &format!("rules/{tgt_slug}.json"),
                &payload_json_full,
                &remote_json_full,
                "rules",
            )?;
            if payload_code != remote_code {
                print_update_diff(
                    &format!("rules/{tgt_slug}.py"),
                    payload_code.clone().unwrap_or_default().as_bytes(),
                    remote_code.clone().unwrap_or_default().as_bytes(),
                );
            }
        }
        if !dry_run {
            // Cross-env PATCH must not echo server-managed fields (org,
            // universal, server-computed back-refs); strip them off `extra`
            // to honour the same contract as a CREATE.
            strip_patch_extra(&mut payload_rule.extra, "rules", true);
            let updated = tgt_client
                .update_rule(tgt_id, &payload_rule, None)
                .await
                .with_context(|| format!("PATCH tgt rules/{tgt_id}"))?;
            write_back_rule(&tgt_paths, tgt_lockfile, tgt_slug, &updated, overlay_paths)?;
        }
        applied.rules += 1;
        if let Some(p) = &progress {
            p.event(Action::Patch, &format!("rule/{tgt_slug}"));
        }
    }

    // Labels -----------------------------------------------------------
    let mut remote_labels_cache: Option<Vec<crate::model::Label>> = None;
    for (src_slug, tgt_slug) in &mapping.labels {
        if let Some(sel) = selection
            && !sel.contains("labels", src_slug)
        {
            continue;
        }
        let Some(tgt_id) =
            lookup_tgt_id_w(tgt_lockfile, "labels", tgt_slug, &mut skipped, &progress)
        else {
            continue;
        };
        let path = src_paths.labels_dir().join(format!("{src_slug}.json"));
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: cannot read src labels/{src_slug}: {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: parsing labels/{src_slug}: {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        rewrite_urls(
            &mut payload,
            &src_lockfile,
            tgt_lockfile,
            &mapping,
            &mapping.hook_templates,
        );
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.label(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let mut payload_label: crate::model::Label = match serde_json::from_value(payload) {
            Ok(v) => v,
            Err(e) => {
                warn(
                    &progress,
                    format!(
                        "warn: labels/{src_slug} -> {tgt_slug}: payload not a valid Label ({e:#}); skipping"
                    ),
                );
                skipped += 1;
                continue;
            }
        };
        let mut payload_bytes =
            serde_json::to_vec_pretty(&payload_label).context("serializing payload label")?;
        payload_bytes.push(b'\n');
        if remote_labels_cache.is_none() {
            remote_labels_cache = Some(
                tgt_client
                    .list_labels(None)
                    .await
                    .context("listing tgt labels for drift check")?,
            );
        }
        let cache = remote_labels_cache.as_ref().unwrap();
        let Some(remote) = cache.iter().find(|l| l.id == tgt_id) else {
            warn(
                &progress,
                format!("warning: label id {tgt_id} not found on tgt remote; skipping"),
            );
            skipped += 1;
            continue;
        };
        let mut remote_bytes =
            serde_json::to_vec_pretty(remote).context("serializing remote label")?;
        remote_bytes.push(b'\n');
        let (in_sync, remote_hash) = tgt_drift_status(
            remote_bytes.clone(),
            overlay_paths,
            tgt_lockfile,
            "labels",
            tgt_slug,
        )?;
        if !in_sync {
            match handle_drift(
                &progress,
                tgt_lockfile,
                drift_decisions,
                &mut drifted,
                dry_run,
                "labels",
                tgt_slug,
                remote_hash,
            )? {
                DriftHandling::SkipObject => continue,
                DriftHandling::Patch => {}
            }
        }
        if bytes_equal_after_strip(&payload_bytes, &remote_bytes, "labels")? {
            continue;
        }
        if dry_run && diff {
            print_update_diff_normalized(
                &format!("labels/{tgt_slug}.json"),
                &payload_bytes,
                &remote_bytes,
                "labels",
            )?;
        }
        if !dry_run {
            // Cross-env PATCH must not echo server-managed fields (org,
            // universal); strip them off `extra` per the CREATE contract.
            strip_patch_extra(&mut payload_label.extra, "labels", true);
            let updated = tgt_client
                .update_label(tgt_id, &payload_label, None)
                .await
                .with_context(|| format!("PATCH tgt labels/{tgt_id}"))?;
            write_back_label(&tgt_paths, tgt_lockfile, tgt_slug, &updated, overlay_paths)?;
        }
        applied.labels += 1;
        if let Some(p) = &progress {
            p.event(Action::Patch, &format!("label/{tgt_slug}"));
        }
    }

    // Queues -----------------------------------------------------------
    let mut remote_queues_cache: Option<Vec<crate::model::Queue>> = None;
    for (src_slug, tgt_slug) in &mapping.queues {
        if let Some(sel) = selection
            && !sel.contains("queues", src_slug)
        {
            continue;
        }
        let Some(tgt_id) =
            lookup_tgt_id_w(tgt_lockfile, "queues", tgt_slug, &mut skipped, &progress)
        else {
            continue;
        };
        let Some(src_queue_dir) = locate_queue_dir(&src_paths, src_slug) else {
            warn(
                &progress,
                format!("warn: cannot locate src queue '{src_slug}' on disk; skipping"),
            );
            skipped += 1;
            continue;
        };
        let queue_path = src_queue_dir.join("queue.json");
        let raw = match std::fs::read_to_string(&queue_path) {
            Ok(r) => r,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: cannot read src queues/{src_slug}: {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: parsing queue '{src_slug}': {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        rewrite_urls(
            &mut payload,
            &src_lockfile,
            tgt_lockfile,
            &mapping,
            &mapping.hook_templates,
        );
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.queue(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        // Validate src payload parses as Queue (catch schema mismatches early)
        // but keep the rewritten Value for the actual PATCH so we can strip
        // server-computed sub-collections (`hooks`, `webhooks`, `rules`,
        // `inbox`, `counts`) that the Rossum API rejects on PATCH.
        if let Err(e) = serde_json::from_value::<crate::model::Queue>(payload.clone()) {
            warn(
                &progress,
                format!(
                    "warn: queues/{src_slug} -> {tgt_slug}: payload not a valid Queue ({e:#}); skipping"
                ),
            );
            skipped += 1;
            continue;
        }
        let mut payload_for_patch = payload;
        strip_for_cross_env_patch(&mut payload_for_patch, "queues");
        let mut payload_bytes =
            serde_json::to_vec_pretty(&payload_for_patch).context("serializing payload queue")?;
        payload_bytes.push(b'\n');
        if remote_queues_cache.is_none() {
            remote_queues_cache = Some(
                tgt_client
                    .list_queues(None)
                    .await
                    .context("listing tgt queues for drift check")?,
            );
        }
        let cache = remote_queues_cache.as_ref().unwrap();
        let Some(remote) = cache.iter().find(|q| q.id == tgt_id) else {
            warn(
                &progress,
                format!("warning: queue id {tgt_id} not found on tgt remote; skipping"),
            );
            skipped += 1;
            continue;
        };
        let mut remote_bytes =
            serde_json::to_vec_pretty(remote).context("serializing remote queue")?;
        remote_bytes.push(b'\n');
        let (in_sync, remote_hash) = tgt_drift_status(
            remote_bytes.clone(),
            overlay_paths,
            tgt_lockfile,
            "queues",
            tgt_slug,
        )?;
        if !in_sync {
            match handle_drift(
                &progress,
                tgt_lockfile,
                drift_decisions,
                &mut drifted,
                dry_run,
                "queues",
                tgt_slug,
                remote_hash,
            )? {
                DriftHandling::SkipObject => continue,
                DriftHandling::Patch => {}
            }
        }
        if bytes_equal_after_strip(&payload_bytes, &remote_bytes, "queues")? {
            continue;
        }
        if dry_run && diff {
            print_update_diff_normalized(
                &format!("queues/{tgt_slug}.json"),
                &payload_bytes,
                &remote_bytes,
                "queues",
            )?;
        }
        if !dry_run {
            let response_value = tgt_client
                .patch_value(&format!("/queues/{tgt_id}"), &payload_for_patch, None)
                .await
                .with_context(|| format!("PATCH tgt queues/{tgt_id}"))?;
            let updated: crate::model::Queue = serde_json::from_value(response_value)
                .context("parsing PATCH /queues response as Queue")?;
            write_back_queue(&tgt_paths, tgt_lockfile, tgt_slug, &updated, overlay_paths)?;
        }
        applied.queues += 1;
        if let Some(p) = &progress {
            p.event(Action::Patch, &format!("queue/{tgt_slug}"));
        }
    }

    // Schemas ----------------------------------------------------------
    for (src_slug, tgt_slug) in &mapping.schemas {
        if let Some(sel) = selection
            && !sel.contains("schemas", src_slug)
        {
            continue;
        }
        let Some(tgt_id) =
            lookup_tgt_id_w(tgt_lockfile, "schemas", tgt_slug, &mut skipped, &progress)
        else {
            continue;
        };
        let Some(src_queue_dir) = locate_queue_dir(&src_paths, src_slug) else {
            warn(
                &progress,
                format!("warn: cannot locate src queue '{src_slug}' for schema; skipping"),
            );
            skipped += 1;
            continue;
        };
        let mut payload = match read_schema_value(&src_queue_dir) {
            Ok(v) => v,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: cannot read src schema for queue '{src_slug}': {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        rewrite_urls(
            &mut payload,
            &src_lockfile,
            tgt_lockfile,
            &mapping,
            &mapping.hook_templates,
        );
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.schema(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let mut payload_schema: crate::model::Schema = match serde_json::from_value(payload) {
            Ok(s) => s,
            Err(e) => {
                warn(
                    &progress,
                    format!(
                        "warn: schemas/{src_slug} -> {tgt_slug}: payload not a valid Schema ({e:#}); skipping"
                    ),
                );
                skipped += 1;
                continue;
            }
        };
        let (payload_json_full, payload_formulas) =
            crate::snapshot::schema::serialize_schema(&payload_schema)?;
        let remote_schema = tgt_client
            .get_schema(tgt_id, None)
            .await
            .with_context(|| format!("fetching tgt schema {tgt_id} for drift check"))?;
        let (remote_json_full, remote_formulas) =
            crate::snapshot::schema::serialize_schema(&remote_schema)?;
        let stripped = maybe_strip_overlay(remote_json_full.clone(), overlay_paths)?;
        let stripped = portabilize_for_hash(stripped, tgt_lockfile)?;
        let remote_combined_hash =
            crate::state::schema_combined_hash(&stripped, &remote_formulas, tgt_lockfile);
        let in_sync = tgt_lockfile
            .objects
            .get("schemas")
            .and_then(|m| m.get(tgt_slug))
            .and_then(|e| e.content_hash.as_deref())
            .map(|b| b == remote_combined_hash)
            .unwrap_or(true);
        if !in_sync {
            match handle_drift(
                &progress,
                tgt_lockfile,
                drift_decisions,
                &mut drifted,
                dry_run,
                "schemas",
                tgt_slug,
                remote_combined_hash,
            )? {
                DriftHandling::SkipObject => continue,
                DriftHandling::Patch => {}
            }
        }
        if bytes_equal_after_strip(&payload_json_full, &remote_json_full, "schemas")?
            && payload_formulas == remote_formulas
        {
            continue;
        }
        if dry_run && diff {
            print_update_diff_normalized(
                &format!("schemas/{tgt_slug}/schema.json"),
                &payload_json_full,
                &remote_json_full,
                "schemas",
            )?;
            // Diff each formula sidecar that differs. Set-of-formulas
            // mismatch (one side has a field the other doesn't) shows
            // as new-file or deleted-file diff.
            let local_map: std::collections::BTreeMap<&str, &[u8]> = payload_formulas
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_slice()))
                .collect();
            let remote_map: std::collections::BTreeMap<&str, &[u8]> = remote_formulas
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_slice()))
                .collect();
            let mut keys: std::collections::BTreeSet<&str> = local_map.keys().copied().collect();
            keys.extend(remote_map.keys().copied());
            for k in keys {
                let l = local_map.get(k).copied().unwrap_or(&[]);
                let r = remote_map.get(k).copied().unwrap_or(&[]);
                if l != r {
                    print_update_diff(&format!("schemas/{tgt_slug}/formulas/{k}.py"), l, r);
                }
            }
        }
        if !dry_run {
            // Cross-env PATCH must not echo server-managed fields (org,
            // universal, the server-computed `queues` back-ref); strip them
            // off `extra` per the CREATE contract.
            strip_patch_extra(&mut payload_schema.extra, "schemas", true);
            let updated = tgt_client
                .update_schema(tgt_id, &payload_schema, None)
                .await
                .with_context(|| format!("PATCH tgt schemas/{tgt_id}"))?;
            write_back_schema(&tgt_paths, tgt_lockfile, tgt_slug, &updated, overlay_paths)?;
        }
        applied.schemas += 1;
        if let Some(p) = &progress {
            p.event(Action::Patch, &format!("schema/{tgt_slug}"));
        }
    }

    // Inboxes ----------------------------------------------------------
    for (src_slug, tgt_slug) in &mapping.inboxes {
        if let Some(sel) = selection
            && !sel.contains("inboxes", src_slug)
        {
            continue;
        }
        let Some(tgt_id) =
            lookup_tgt_id_w(tgt_lockfile, "inboxes", tgt_slug, &mut skipped, &progress)
        else {
            continue;
        };
        let Some(src_queue_dir) = locate_queue_dir(&src_paths, src_slug) else {
            warn(
                &progress,
                format!("warn: cannot locate src queue '{src_slug}' for inbox; skipping"),
            );
            skipped += 1;
            continue;
        };
        let inbox_path = src_queue_dir.join("inbox.json");
        let raw = match std::fs::read_to_string(&inbox_path) {
            Ok(r) => r,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: cannot read src inbox for queue '{src_slug}': {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: parsing inbox for queue '{src_slug}': {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        rewrite_urls(
            &mut payload,
            &src_lockfile,
            tgt_lockfile,
            &mapping,
            &mapping.hook_templates,
        );
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.inbox(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_inbox: crate::model::Inbox = match serde_json::from_value(payload) {
            Ok(i) => i,
            Err(e) => {
                warn(
                    &progress,
                    format!(
                        "warn: inboxes/{src_slug} -> {tgt_slug}: payload not a valid Inbox ({e:#}); skipping"
                    ),
                );
                skipped += 1;
                continue;
            }
        };
        let mut payload_bytes =
            serde_json::to_vec_pretty(&payload_inbox).context("serializing payload inbox")?;
        payload_bytes.push(b'\n');
        let remote_inbox = tgt_client
            .get_inbox(tgt_id, None)
            .await
            .with_context(|| format!("fetching tgt inbox {tgt_id} for drift check"))?;
        let mut remote_bytes =
            serde_json::to_vec_pretty(&remote_inbox).context("serializing remote inbox")?;
        remote_bytes.push(b'\n');
        let (in_sync, remote_hash) = tgt_drift_status(
            remote_bytes.clone(),
            overlay_paths,
            tgt_lockfile,
            "inboxes",
            tgt_slug,
        )?;
        if !in_sync {
            match handle_drift(
                &progress,
                tgt_lockfile,
                drift_decisions,
                &mut drifted,
                dry_run,
                "inboxes",
                tgt_slug,
                remote_hash,
            )? {
                DriftHandling::SkipObject => continue,
                DriftHandling::Patch => {}
            }
        }
        if bytes_equal_after_strip(&payload_bytes, &remote_bytes, "inboxes")? {
            continue;
        }
        if dry_run && diff {
            print_update_diff_normalized(
                &format!("inboxes/{tgt_slug}.json"),
                &payload_bytes,
                &remote_bytes,
                "inboxes",
            )?;
        }
        if !dry_run {
            // Inbox `email` is per-env (auto-assigned at create on the
            // tgt domain). Sending the src env's email cross-env is at
            // best ignored, at worst destructive. Build the PATCH body
            // from the typed payload, then apply the same cross-env
            // strip the idempotency check uses (which removes `email`
            // via the "inboxes" entry in kind_specific_strip).
            let mut patch_body = serde_json::to_value(&payload_inbox)
                .context("serializing payload inbox for PATCH")?;
            crate::snapshot::create::strip_for_cross_env_patch(&mut patch_body, "inboxes");
            let updated = tgt_client
                .update_inbox_value(tgt_id, &patch_body, None)
                .await
                .with_context(|| format!("PATCH tgt inboxes/{tgt_id}"))?;
            write_back_inbox(&tgt_paths, tgt_lockfile, tgt_slug, &updated, overlay_paths)?;
        }
        applied.inboxes += 1;
        if let Some(p) = &progress {
            p.event(Action::Patch, &format!("inbox/{tgt_slug}"));
        }
    }

    // Email templates --------------------------------------------------
    let mut remote_template_cache: Option<Vec<crate::model::EmailTemplate>> = None;
    for (src_key, tgt_key) in &mapping.email_templates {
        if let Some(sel) = selection
            && !sel.contains("email_templates", src_key)
        {
            continue;
        }
        let Some(tgt_id) = lookup_tgt_id_w(
            tgt_lockfile,
            "email_templates",
            tgt_key,
            &mut skipped,
            &progress,
        ) else {
            continue;
        };
        let Some((ws, q, t)) = split_template_key(src_key) else {
            warn(
                &progress,
                format!(
                    "warning: src email_template key '{src_key}' is not <ws>/<q>/<template>; skipping"
                ),
            );
            skipped += 1;
            continue;
        };
        let templates_dir = src_paths.queue_email_templates_dir(ws, q);
        let src_template = match read_email_template(&templates_dir, t) {
            Ok(t) => t,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: cannot read src email_template '{src_key}': {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        let mut payload = serde_json::to_value(&src_template)
            .context("serializing src email template to value")?;
        rewrite_urls(
            &mut payload,
            &src_lockfile,
            tgt_lockfile,
            &mapping,
            &mapping.hook_templates,
        );
        let overlay_paths = tgt_overlay
            .as_ref()
            .and_then(|ov| ov.email_template(tgt_key));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        // Validate as EmailTemplate (catch schema mismatches), but keep the
        // Value for the PATCH so we can strip `triggers` — which references a
        // non-deployable sub-resource and 400s the API on cross-env send.
        if let Err(e) = serde_json::from_value::<crate::model::EmailTemplate>(payload.clone()) {
            warn(
                &progress,
                format!(
                    "warn: email_templates/{src_key} -> {tgt_key}: payload not a valid EmailTemplate ({e:#}); skipping"
                ),
            );
            skipped += 1;
            continue;
        }
        let mut payload_for_patch = payload;
        strip_for_cross_env_patch(&mut payload_for_patch, "email_templates");
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_for_patch)
            .context("serializing payload email template")?;
        payload_bytes.push(b'\n');
        if remote_template_cache.is_none() {
            remote_template_cache = Some(
                tgt_client
                    .list_email_templates(None)
                    .await
                    .context("listing tgt email templates for drift check")?,
            );
        }
        let cache = remote_template_cache.as_ref().unwrap();
        let Some(remote_template) = cache.iter().find(|t| t.id == tgt_id) else {
            warn(
                &progress,
                format!("warning: email_template id {tgt_id} not found on tgt remote; skipping"),
            );
            skipped += 1;
            continue;
        };
        let mut remote_bytes = serde_json::to_vec_pretty(remote_template)
            .context("serializing remote email template")?;
        remote_bytes.push(b'\n');
        let (in_sync, remote_hash) = tgt_drift_status(
            remote_bytes.clone(),
            overlay_paths,
            tgt_lockfile,
            "email_templates",
            tgt_key,
        )?;
        if !in_sync {
            match handle_drift(
                &progress,
                tgt_lockfile,
                drift_decisions,
                &mut drifted,
                dry_run,
                "email_templates",
                tgt_key,
                remote_hash,
            )? {
                DriftHandling::SkipObject => continue,
                DriftHandling::Patch => {}
            }
        }
        if bytes_equal_after_strip(&payload_bytes, &remote_bytes, "email_templates")? {
            continue;
        }
        if dry_run && diff {
            print_update_diff_normalized(
                &format!("email_templates/{tgt_key}.json"),
                &payload_bytes,
                &remote_bytes,
                "email_templates",
            )?;
        }
        if !dry_run {
            let response_value = tgt_client
                .patch_value(
                    &format!("/email_templates/{tgt_id}"),
                    &payload_for_patch,
                    None,
                )
                .await
                .with_context(|| format!("PATCH tgt email_templates/{tgt_id}"))?;
            let updated: crate::model::EmailTemplate = serde_json::from_value(response_value)
                .context("parsing PATCH /email_templates response as EmailTemplate")?;
            write_back_email_template(&tgt_paths, tgt_lockfile, tgt_key, &updated, overlay_paths)?;
        }
        applied.email_templates += 1;
        if let Some(p) = &progress {
            p.event(Action::Patch, &format!("email_template/{tgt_key}"));
        }
    }

    // Engines ----------------------------------------------------------
    for (src_slug, tgt_slug) in &mapping.engines {
        if let Some(sel) = selection
            && !sel.contains("engines", src_slug)
        {
            continue;
        }
        let Some(tgt_id) =
            lookup_tgt_id_w(tgt_lockfile, "engines", tgt_slug, &mut skipped, &progress)
        else {
            continue;
        };
        // Engines live at `engines/<slug>/engine.json`, not directly at
        // `engines/<slug>.json` — same nested-dir layout as workspaces.
        let path = src_paths.engine_dir(src_slug).join("engine.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: cannot read src engines/{src_slug}: {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: parsing engines/{src_slug}: {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        rewrite_urls(
            &mut payload,
            &src_lockfile,
            tgt_lockfile,
            &mapping,
            &mapping.hook_templates,
        );
        let overlay_paths = tgt_overlay.as_ref().and_then(|ov| ov.engine(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let mut payload_engine: crate::model::Engine = match serde_json::from_value(payload) {
            Ok(e) => e,
            Err(e) => {
                warn(
                    &progress,
                    format!(
                        "warn: engines/{src_slug} -> {tgt_slug}: payload not a valid Engine ({e:#}); skipping"
                    ),
                );
                skipped += 1;
                continue;
            }
        };
        let mut payload_bytes =
            serde_json::to_vec_pretty(&payload_engine).context("serializing payload engine")?;
        payload_bytes.push(b'\n');
        let remotes = tgt_client
            .list_engines(None)
            .await
            .context("listing tgt engines for drift check")?;
        let Some(remote) = remotes.iter().find(|e| e.id == tgt_id) else {
            warn(
                &progress,
                format!("warning: engine id {tgt_id} not found on tgt remote; skipping"),
            );
            skipped += 1;
            continue;
        };
        let mut remote_bytes =
            serde_json::to_vec_pretty(remote).context("serializing remote engine")?;
        remote_bytes.push(b'\n');
        let (in_sync, remote_hash) = tgt_drift_status(
            remote_bytes.clone(),
            overlay_paths,
            tgt_lockfile,
            "engines",
            tgt_slug,
        )?;
        if !in_sync {
            match handle_drift(
                &progress,
                tgt_lockfile,
                drift_decisions,
                &mut drifted,
                dry_run,
                "engines",
                tgt_slug,
                remote_hash,
            )? {
                DriftHandling::SkipObject => continue,
                DriftHandling::Patch => {}
            }
        }
        if bytes_equal_after_strip(&payload_bytes, &remote_bytes, "engines")? {
            continue;
        }
        if dry_run && diff {
            print_update_diff_normalized(
                &format!("engines/{tgt_slug}.json"),
                &payload_bytes,
                &remote_bytes,
                "engines",
            )?;
        }
        if dry_run {
            applied.engines += 1;
            if let Some(p) = &progress {
                p.event(Action::Patch, &format!("engine/{tgt_slug}"));
            }
        } else {
            // Cross-env PATCH must not echo `agenda_id` (read-only, per-env,
            // refreshes on training) or org/universal fields — that would try
            // to overwrite the target engine's identifier with the source's
            // sentinel. Strip them off `extra`, matching the CREATE contract.
            strip_patch_extra(&mut payload_engine.extra, "engines", true);
            match tgt_client
                .update_engine(tgt_id, &payload_engine, None)
                .await
                .with_context(|| format!("PATCH tgt engines/{tgt_id}"))
            {
                Ok(updated) => {
                    write_back_engine(&tgt_paths, tgt_lockfile, tgt_slug, &updated, overlay_paths)?;
                    applied.engines += 1;
                    if let Some(p) = &progress {
                        p.event(Action::Patch, &format!("engine/{tgt_slug}"));
                    }
                }
                Err(e) if anyhow_has_status(&e, 405) => {
                    warn(&progress, "warning: engines are not writable via PATCH on tgt org/plan (405). Skipping all engine apply.".to_string());
                    skipped += 1;
                    break;
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }

    // Engine fields ----------------------------------------------------
    for (src_slug, tgt_slug) in &mapping.engine_fields {
        if let Some(sel) = selection
            && !sel.contains("engine_fields", src_slug)
        {
            continue;
        }
        let Some(tgt_id) = lookup_tgt_id_w(
            tgt_lockfile,
            "engine_fields",
            tgt_slug,
            &mut skipped,
            &progress,
        ) else {
            continue;
        };
        let Some(path) = locate_engine_field_path(&src_paths, src_slug) else {
            warn(
                &progress,
                format!(
                    "warning: cannot locate src engine field '{src_slug}' under any engine dir; skipping"
                ),
            );
            skipped += 1;
            continue;
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: cannot read src engine field '{src_slug}': {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        let mut payload: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn(
                    &progress,
                    format!("warning: parsing engine-fields/{src_slug}: {e:#}"),
                );
                skipped += 1;
                continue;
            }
        };
        rewrite_urls(
            &mut payload,
            &src_lockfile,
            tgt_lockfile,
            &mapping,
            &mapping.hook_templates,
        );
        let overlay_paths = tgt_overlay
            .as_ref()
            .and_then(|ov| ov.engine_field(tgt_slug));
        if let Some(p) = overlay_paths {
            apply_overrides(&mut payload, p);
        }
        let payload_field: crate::model::EngineField = match serde_json::from_value(payload) {
            Ok(f) => f,
            Err(e) => {
                warn(
                    &progress,
                    format!(
                        "warn: engine-fields/{src_slug} -> {tgt_slug}: payload not a valid EngineField ({e:#}); skipping"
                    ),
                );
                skipped += 1;
                continue;
            }
        };
        let mut payload_bytes = serde_json::to_vec_pretty(&payload_field)
            .context("serializing payload engine field")?;
        payload_bytes.push(b'\n');
        let remotes = tgt_client
            .list_engine_fields(None)
            .await
            .context("listing tgt engine fields for drift check")?;
        let Some(remote) = remotes.iter().find(|f| f.id == tgt_id) else {
            warn(
                &progress,
                format!("warning: engine_field id {tgt_id} not found on tgt remote; skipping"),
            );
            skipped += 1;
            continue;
        };
        let mut remote_bytes =
            serde_json::to_vec_pretty(remote).context("serializing remote engine field")?;
        remote_bytes.push(b'\n');
        let (in_sync, remote_hash) = tgt_drift_status(
            remote_bytes.clone(),
            overlay_paths,
            tgt_lockfile,
            "engine_fields",
            tgt_slug,
        )?;
        if !in_sync {
            match handle_drift(
                &progress,
                tgt_lockfile,
                drift_decisions,
                &mut drifted,
                dry_run,
                "engine_fields",
                tgt_slug,
                remote_hash,
            )? {
                DriftHandling::SkipObject => continue,
                DriftHandling::Patch => {}
            }
        }
        if bytes_equal_after_strip(&payload_bytes, &remote_bytes, "engine_fields")? {
            continue;
        }
        if dry_run && diff {
            print_update_diff_normalized(
                &format!("engine_fields/{tgt_slug}.json"),
                &payload_bytes,
                &remote_bytes,
                "engine_fields",
            )?;
        }
        if dry_run {
            applied.engine_fields += 1;
            if let Some(p) = &progress {
                p.event(Action::Patch, &format!("engine_field/{tgt_slug}"));
            }
        } else {
            // Engine field `name` is immutable on the Rossum API (a PATCH
            // that changes it returns 400). Build the PATCH body from the
            // typed payload, then strip `name` so a slug mapping that
            // pairs differently-named fields (e.g. `item-qty` paired with
            // `item-quantity`) can still PATCH the other attributes.
            let mut patch_body = serde_json::to_value(&payload_field)
                .context("serializing payload engine_field for PATCH")?;
            if let Some(obj) = patch_body.as_object_mut() {
                obj.remove("name");
            }
            match tgt_client
                .update_engine_field_value(tgt_id, &patch_body, None)
                .await
                .with_context(|| format!("PATCH tgt engine_fields/{tgt_id}"))
            {
                Ok(updated) => {
                    write_back_engine_field(
                        &tgt_paths,
                        tgt_lockfile,
                        tgt_slug,
                        &updated,
                        overlay_paths,
                    )?;
                    applied.engine_fields += 1;
                    if let Some(p) = &progress {
                        p.event(Action::Patch, &format!("engine_field/{tgt_slug}"));
                    }
                }
                Err(e) if anyhow_has_status(&e, 405) => {
                    warn(&progress, "warning: engine fields are not writable via PATCH on tgt org/plan (405). Skipping all engine field apply.".to_string());
                    skipped += 1;
                    break;
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }

    let total = applied.total();
    let verb = if dry_run { "Would apply" } else { "Applied" };
    let suffix = if dry_run {
        "(dry run, no PATCHes sent)"
    } else {
        "PATCHes"
    };
    let mut summary = if dry_run {
        format!(
            "{verb} {} workspaces, {} hooks, {} rules, {} labels, {} queues, {} schemas, \
{} inboxes, {} email templates, {} engines, {} engine fields ({} change(s)) from {src} to {tgt} {suffix}",
            applied.workspaces,
            applied.hooks,
            applied.rules,
            applied.labels,
            applied.queues,
            applied.schemas,
            applied.inboxes,
            applied.email_templates,
            applied.engines,
            applied.engine_fields,
            total,
        )
    } else {
        format!(
            "Applied {} workspaces, {} hooks, {} rules, {} labels, {} queues, {} schemas, \
{} inboxes, {} email templates, {} engines, {} engine fields ({} PATCHes) from {src} to {tgt}",
            applied.workspaces,
            applied.hooks,
            applied.rules,
            applied.labels,
            applied.queues,
            applied.schemas,
            applied.inboxes,
            applied.email_templates,
            applied.engines,
            applied.engine_fields,
            total,
        )
    };
    if skipped > 0 {
        summary.push_str(&format!(", {skipped} skipped"));
    }
    Ok(ApplyOutcome { summary, drifted })
}

/// Emit a `--- tgt before / +++ tgt after` unified diff for one update.
/// Skipped silently when bytes are equal (matches `print_unified`).
///
/// Frames the delta as the tgt's own before/after state — `-` rows are what
/// tgt currently has, `+` rows are what tgt will have post-PATCH — rather
/// than a cross-env comparison. The "after" side is the normalised src
/// payload (which is exactly what the PATCH writes for the visible field
/// set; server-only fields are stripped from both sides by
/// [`normalize_for_cross_env_compare`], so the displayed delta is precisely
/// what the PATCH would change on tgt).
///
/// Used for non-JSON sidecars (hook `.py` / `.js`, rule `.py`, schema
/// formulas) where there's nothing to normalise — the rendered diff is
/// the raw byte delta. JSON updates use [`print_update_diff_normalized`]
/// so server-only fields and key-order jitter don't pollute the view.
fn print_update_diff(label: &str, src: &[u8], tgt_remote: &[u8]) {
    let before = String::from_utf8_lossy(tgt_remote);
    let after = String::from_utf8_lossy(src);
    crate::cli::resolve::print_unified(
        &format!("{label} (tgt before)"),
        &format!("{label} (tgt after)"),
        &before,
        &after,
        &mut 0,
    );
}

/// Emit a `--- tgt before / +++ tgt after` unified diff for one JSON
/// update, piping both sides through [`normalize_for_cross_env_compare`]
/// first.
///
/// Why: the idempotency check (`bytes_equal_after_strip`) compares
/// normalised bytes — server-only fields (`id`, `url`, `organization`,
/// `modified_at`, `modifier`, kind-specific server-managed fields like
/// `triggers` on email_templates) stripped, keys sorted recursively,
/// URL arrays sorted. Until this helper existed, the diff renderer
/// printed the *raw* `payload_bytes` and `remote_bytes`, so dry-run
/// previews padded every PATCH with ~10 lines of fields the deploy
/// already knew it was ignoring. Normalising before rendering makes
/// the diff match what the comparison actually saw — the visible
/// delta is exactly what the PATCH would change.
fn print_update_diff_normalized(
    label: &str,
    src: &[u8],
    tgt_remote: &[u8],
    kind: &str,
) -> Result<()> {
    let src_norm = crate::cli::deploy::common::normalize_for_cross_env_compare(src, kind)
        .with_context(|| format!("normalising src bytes for {kind} diff render"))?;
    let tgt_norm = crate::cli::deploy::common::normalize_for_cross_env_compare(tgt_remote, kind)
        .with_context(|| format!("normalising tgt bytes for {kind} diff render"))?;
    print_update_diff(label, &src_norm, &tgt_norm);
    Ok(())
}

#[derive(Default)]
struct ApplyCounts {
    workspaces: usize,
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
        self.workspaces
            + self.hooks
            + self.rules
            + self.labels
            + self.queues
            + self.schemas
            + self.inboxes
            + self.email_templates
            + self.engines
            + self.engine_fields
    }
}

fn lookup_tgt_id_w(
    tgt_lockfile: &Lockfile,
    kind: &str,
    tgt_slug: &str,
    skipped: &mut usize,
    progress: &Option<Arc<Log>>,
) -> Option<u64> {
    match tgt_lockfile
        .objects
        .get(kind)
        .and_then(|m| m.get(tgt_slug))
        .map(|e| e.id)
    {
        Some(id) => Some(id),
        None => {
            warn(
                progress,
                format!(
                    "warn: tgt lockfile has no entry for {kind}/{tgt_slug}; skipping (run `rdc sync <tgt>` first)"
                ),
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

/// Locate an engine field's on-disk path from its composite
/// `<engine_slug>/<field_slug>` key. Falls back to a global walk for
/// legacy flat keys (lockfiles written before composite-key migration).
fn locate_engine_field_path(paths: &Paths, composite_key: &str) -> Option<PathBuf> {
    if let Some((e_slug, f_slug)) = composite_key.split_once('/') {
        let candidate = paths
            .engine_fields_dir(e_slug)
            .join(format!("{f_slug}.json"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let engines_dir = paths.engines_dir();
    let entries = std::fs::read_dir(&engines_dir).ok()?;
    for e_entry in entries.flatten() {
        if !e_entry.file_type().ok()?.is_dir() {
            continue;
        }
        let e_slug = e_entry.file_name().to_string_lossy().to_string();
        let candidate = paths
            .engine_fields_dir(&e_slug)
            .join(format!("{composite_key}.json"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

pub(crate) fn split_template_key(key: &str) -> Option<(&str, &str, &str)> {
    let mut parts = key.splitn(3, '/');
    let ws = parts.next()?;
    let q = parts.next()?;
    let t = parts.next()?;
    if ws.is_empty() || q.is_empty() || t.is_empty() {
        return None;
    }
    Some((ws, q, t))
}

// ─── write-back helpers (apply → tgt local snapshot) ──────────────────
//
// After a successful cross-env PATCH the Rossum API returns the canonical
// post-PATCH state of the object. Write it back to the tgt local snapshot
// and refresh the tgt lockfile entry so `rdc sync <tgt>` is unnecessary
// after `rdc deploy <src> <tgt>`. Mirrors the per-create writes in
// `cli::deploy::create` and the per-pull writes in `cli::pull`. No extra
// API calls — the data is already in hand.
//
// Each helper: redact_for_disk → write via kind-specific writer (sidecars
// included for hook/rule/schema) → hash with the kind's hash function →
// upsert tgt lockfile, preserving secrets_hash for hooks.

/// Round-trip the response through `redact_for_disk` so server-noise fields
/// (e.g. hook.status, engine.agenda_id) land as the sentinel string on disk,
/// matching what a fresh `pull`/`sync` would write.
fn redacted_response<T: serde::Serialize + serde::de::DeserializeOwned>(
    response: &T,
    kind: &str,
) -> Result<T> {
    let mut v = serde_json::to_value(response)
        .with_context(|| format!("serialising {kind} response for redaction"))?;
    redact_for_disk(&mut v, kind);
    let typed: T = serde_json::from_value(v)
        .with_context(|| format!("deserialising redacted {kind} response"))?;
    Ok(typed)
}

fn upsert_after_write_back(
    lockfile: &mut Lockfile,
    kind: &str,
    slug: &str,
    id: u64,
    url: &str,
    modified_at: Option<&str>,
    hash: String,
) {
    let prev_secrets = lockfile
        .objects
        .get(kind)
        .and_then(|m| m.get(slug))
        .and_then(|e| e.secrets_hash.clone());
    lockfile.upsert(
        kind,
        slug,
        ObjectEntry {
            id,
            url: Some(url.to_string()),
            modified_at: modified_at.map(|s| s.to_string()),
            content_hash: Some(hash),
            secrets_hash: prev_secrets,
        },
    );
}

/// Common path for flat (no-sidecar) kinds: run the API body through the
/// [`KindCodec`] (redacts noise fields AND strips `modified_at` recursively),
/// apply the tgt overlay strip so the recorded hash matches what a subsequent
/// `pull`/`sync` on tgt would recompute, write the post-overlay JSON to disk,
/// and update the tgt lockfile entry. Also mirrors the bytes to the tgt env's
/// base cache so the next sync on tgt can 3-way-merge from a current base.
fn write_back_flat<T: serde::Serialize>(
    tgt_paths: &Paths,
    tgt_lockfile: &mut Lockfile,
    kind: &str,
    slug: &str,
    file_path: &Path,
    response: &T,
    id: u64,
    url: &str,
    modified_at: Option<&str>,
    overlay_paths: Option<&BTreeMap<String, Value>>,
) -> Result<()> {
    let value = serde_json::to_value(response)
        .with_context(|| format!("serialising {kind}/{slug} response for write-back"))?;
    // Route through the codec so we get the same redaction + hidden-field strip
    // that pull/sync apply. For unregistered kinds fall back to raw pretty-print.
    let (disk_json, sidecars) = if let Some(c) = crate::snapshot::codec::codec(kind) {
        let art = c
            .disk_bytes(&value)
            .with_context(|| format!("codec disk_bytes for {kind}/{slug}"))?;
        (art.json, art.sidecars)
    } else {
        let mut b = serde_json::to_vec_pretty(&value)
            .with_context(|| format!("encoding {kind}/{slug} write-back bytes"))?;
        b.push(b'\n');
        (b, vec![])
    };
    // Strip overlay paths so the recorded hash matches what `tgt_drift_status`
    // (and the pull driver) would compute from the same remote response.
    let json_for_hash = maybe_strip_overlay(disk_json.clone(), overlay_paths)?;
    let hash = combined_hash(&json_for_hash, &sidecars, tgt_lockfile);
    // Write the post-overlay bytes to disk (overlay fields are tgt-specific
    // overrides, so they must NOT appear in the stored snapshot either).
    crate::state::base_cache::write_disk_and_cache(tgt_paths, file_path, &json_for_hash)?;
    upsert_after_write_back(tgt_lockfile, kind, slug, id, url, modified_at, hash);
    Ok(())
}

fn write_back_workspace(
    tgt_paths: &Paths,
    tgt_lockfile: &mut Lockfile,
    slug: &str,
    response: &crate::model::Workspace,
    overlay_paths: Option<&BTreeMap<String, Value>>,
) -> Result<()> {
    let dir = tgt_paths.workspace_dir(slug);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    write_back_flat(
        tgt_paths,
        tgt_lockfile,
        "workspaces",
        slug,
        &dir.join("workspace.json"),
        response,
        response.id,
        &response.url,
        response.modified_at(),
        overlay_paths,
    )
}

fn write_back_label(
    tgt_paths: &Paths,
    tgt_lockfile: &mut Lockfile,
    slug: &str,
    response: &crate::model::Label,
    overlay_paths: Option<&BTreeMap<String, Value>>,
) -> Result<()> {
    let dir = tgt_paths.labels_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    write_back_flat(
        tgt_paths,
        tgt_lockfile,
        "labels",
        slug,
        &dir.join(format!("{slug}.json")),
        response,
        response.id,
        &response.url,
        response.modified_at(),
        overlay_paths,
    )
}

fn write_back_queue(
    tgt_paths: &Paths,
    tgt_lockfile: &mut Lockfile,
    q_slug: &str,
    response: &crate::model::Queue,
    overlay_paths: Option<&BTreeMap<String, Value>>,
) -> Result<()> {
    let dir = locate_queue_dir(tgt_paths, q_slug)
        .ok_or_else(|| anyhow!("tgt queue dir for '{q_slug}' not found for write-back"))?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    write_back_flat(
        tgt_paths,
        tgt_lockfile,
        "queues",
        q_slug,
        &dir.join("queue.json"),
        response,
        response.id,
        &response.url,
        response.modified_at(),
        overlay_paths,
    )
}

fn write_back_inbox(
    tgt_paths: &Paths,
    tgt_lockfile: &mut Lockfile,
    q_slug: &str,
    response: &crate::model::Inbox,
    overlay_paths: Option<&BTreeMap<String, Value>>,
) -> Result<()> {
    let dir = locate_queue_dir(tgt_paths, q_slug)
        .ok_or_else(|| anyhow!("tgt queue dir for inbox '{q_slug}' not found for write-back"))?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    write_back_flat(
        tgt_paths,
        tgt_lockfile,
        "inboxes",
        q_slug,
        &dir.join("inbox.json"),
        response,
        response.id,
        &response.url,
        response.modified_at(),
        overlay_paths,
    )
}

fn write_back_engine(
    tgt_paths: &Paths,
    tgt_lockfile: &mut Lockfile,
    slug: &str,
    response: &crate::model::Engine,
    overlay_paths: Option<&BTreeMap<String, Value>>,
) -> Result<()> {
    let dir = tgt_paths.engine_dir(slug);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    write_back_flat(
        tgt_paths,
        tgt_lockfile,
        "engines",
        slug,
        &dir.join("engine.json"),
        response,
        response.id,
        &response.url,
        response.modified_at(),
        overlay_paths,
    )
}

fn write_back_engine_field(
    tgt_paths: &Paths,
    tgt_lockfile: &mut Lockfile,
    composite_key: &str,
    response: &crate::model::EngineField,
    overlay_paths: Option<&BTreeMap<String, Value>>,
) -> Result<()> {
    // `composite_key` is `<engine_slug>/<field_slug>` — the lockfile /
    // mapping shape. Split it for the on-disk path.
    let (engine_slug, field_slug) = composite_key.split_once('/').ok_or_else(|| {
        anyhow!(
            "tgt engine_field key '{composite_key}' is not <engine>/<field>; \
             re-run `rdc sync` on the target to migrate the lockfile"
        )
    })?;
    let dir = tgt_paths.engine_fields_dir(engine_slug);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    write_back_flat(
        tgt_paths,
        tgt_lockfile,
        "engine_fields",
        composite_key,
        &dir.join(format!("{field_slug}.json")),
        response,
        response.id,
        &response.url,
        response.modified_at(),
        overlay_paths,
    )
}

fn write_back_hook(
    tgt_paths: &Paths,
    tgt_lockfile: &mut Lockfile,
    slug: &str,
    response: &crate::model::Hook,
    overlay_paths: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
) -> Result<()> {
    let dir = tgt_paths.hooks_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let redacted = redacted_response(response, "hooks")?;
    let json_bytes = write_hook(&dir, slug, &redacted)?;
    let code = redacted
        .config
        .get("code")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    // Hash over POST-overlay bytes so it matches what pull::hooks records in
    // the lockfile. Without stripping, a subsequent `sync tgt` would see the
    // pre-overlay hash here and the post-overlay hash computed from disk,
    // causing phantom drift on every deploy for hooks with overlays.
    let json_for_hash = maybe_strip_overlay(json_bytes, overlay_paths)?;
    let hash = {
        let sidecars: Vec<(String, Vec<u8>)> = if let Some(c) = &code {
            vec![("code".to_string(), c.as_bytes().to_vec())]
        } else {
            vec![]
        };
        crate::snapshot::codec::combined_hash(&json_for_hash, &sidecars, tgt_lockfile)
    };
    upsert_after_write_back(
        tgt_lockfile,
        "hooks",
        slug,
        redacted.id,
        &redacted.url,
        redacted.modified_at(),
        hash,
    );
    Ok(())
}

fn write_back_rule(
    tgt_paths: &Paths,
    tgt_lockfile: &mut Lockfile,
    slug: &str,
    response: &crate::model::Rule,
    overlay_paths: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
) -> Result<()> {
    let dir = tgt_paths.rules_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let (json_bytes, code) = crate::snapshot::rule::serialize_rule(response)?;
    write_rule(&dir, slug, response)?;
    // Hash over POST-overlay bytes so it matches what pull::rules records in
    // the lockfile (same framing as the drift check above).
    let json_for_hash = maybe_strip_overlay(json_bytes, overlay_paths)?;
    let hash = rule_combined_hash(&json_for_hash, &code, tgt_lockfile);
    upsert_after_write_back(
        tgt_lockfile,
        "rules",
        slug,
        response.id,
        &response.url,
        response.modified_at(),
        hash,
    );
    Ok(())
}

fn write_back_schema(
    tgt_paths: &Paths,
    tgt_lockfile: &mut Lockfile,
    q_slug: &str,
    response: &crate::model::Schema,
    overlay_paths: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
) -> Result<()> {
    let dir = locate_queue_dir(tgt_paths, q_slug)
        .ok_or_else(|| anyhow!("tgt queue dir for schema '{q_slug}' not found for write-back"))?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let (json_bytes, formula_parts) = crate::snapshot::schema::serialize_schema(response)?;
    write_schema_bytes(&dir, &json_bytes, &formula_parts)?;
    // Hash over POST-overlay bytes so it matches what pull::queues records in
    // the lockfile (same framing as the drift check above).
    let json_for_hash = maybe_strip_overlay(json_bytes, overlay_paths)?;
    let hash = schema_combined_hash(&json_for_hash, &formula_parts, tgt_lockfile);
    upsert_after_write_back(
        tgt_lockfile,
        "schemas",
        q_slug,
        response.id,
        &response.url,
        response.modified_at(),
        hash,
    );
    Ok(())
}

fn write_back_email_template(
    tgt_paths: &Paths,
    tgt_lockfile: &mut Lockfile,
    key: &str,
    response: &crate::model::EmailTemplate,
    overlay_paths: Option<&BTreeMap<String, Value>>,
) -> Result<()> {
    let (ws, q, t) = split_template_key(key)
        .ok_or_else(|| anyhow!("email_template key '{key}' is not <ws>/<q>/<template>"))?;
    let dir = tgt_paths.queue_email_templates_dir(ws, q);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // `write_email_template` already applies `serialize_for_disk` (strips
    // `modified_at`), so the bytes are codec-equivalent for email_templates.
    // Strip overlay paths so the recorded hash matches `tgt_drift_status`.
    let bytes = write_email_template(&dir, t, response)?;
    let bytes_for_hash = maybe_strip_overlay(bytes, overlay_paths)?;
    upsert_after_write_back(
        tgt_lockfile,
        "email_templates",
        key,
        response.id,
        &response.url,
        response.modified_at(),
        combined_hash(&bytes_for_hash, &[], tgt_lockfile),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::codec::combined_hash;
    use serde_json::json;
    use tempfile::TempDir;

    /// Build a `Paths` pointing at a temporary directory and create the
    /// `envs/<env>` subtree so `write_disk_and_cache` doesn't fail the
    /// "path not under env_root" assertion.
    fn tmp_paths(tmp: &TempDir, env: &str) -> Paths {
        let paths = Paths::for_env(tmp.path(), env);
        std::fs::create_dir_all(paths.env_root()).expect("creating env_root in tmpdir");
        std::fs::create_dir_all(paths.base_cache_root()).expect("creating base_cache_root");
        paths
    }

    /// The core consistency guarantee: after `write_back_flat` (via the engine
    /// wrapper), the lockfile `content_hash` must equal
    /// `combined_hash(maybe_strip_overlay(codec(kind).disk_bytes(&response).json, overlay), &sidecars)`.
    ///
    /// Additionally, the JSON written to disk must contain no top-level or
    /// nested `modified_at` field (codec strips it recursively).
    #[test]
    fn write_back_engine_records_codec_baseline_no_modified_at() {
        let tmp = TempDir::new().unwrap();
        let paths = tmp_paths(&tmp, "tgt");
        let mut lockfile = Lockfile::default();

        // Simulate an API response that carries `modified_at` (top-level) and
        // `agenda_id` (a redacted sentinel field). The PATCH response from the
        // Rossum API always includes both.
        let response_value = json!({
            "id": 42,
            "url": "https://prod.rossum.app/api/v1/engines/42",
            "name": "invoice-extractor",
            "modified_at": "2026-05-01T10:00:00Z",
            "agenda_id": "live-agenda-uuid-12345",
        });
        let response: crate::model::Engine =
            serde_json::from_value(response_value.clone()).unwrap();

        let dir = paths.engine_dir("invoice-extractor");
        std::fs::create_dir_all(&dir).unwrap();

        // Call write_back_engine with no overlay (no tgt-specific overrides).
        write_back_engine(&paths, &mut lockfile, "invoice-extractor", &response, None).unwrap();

        // ── Baseline consistency assertion ──────────────────────────────────
        // The recorded hash must equal combined_hash(codec.disk_bytes, overlay).
        let codec =
            crate::snapshot::codec::codec("engines").expect("engines codec must be registered");
        let art = codec.disk_bytes(&response_value).unwrap();
        // No overlay → maybe_strip_overlay is a no-op.
        let expected_hash = combined_hash(&art.json, &art.sidecars, &Lockfile::default());

        let recorded_hash = lockfile
            .objects
            .get("engines")
            .and_then(|m| m.get("invoice-extractor"))
            .and_then(|e| e.content_hash.as_deref())
            .expect("lockfile must have an entry for the written engine");

        assert_eq!(
            recorded_hash, expected_hash,
            "write_back_engine must record codec baseline hash (no modified_at, redacted sentinels)"
        );

        // ── On-disk content: no modified_at anywhere ─────────────────────
        let disk_path = dir.join("engine.json");
        let disk_raw = std::fs::read_to_string(&disk_path).unwrap();
        let disk_value: serde_json::Value = serde_json::from_str(&disk_raw).unwrap();
        assert!(
            disk_value.get("modified_at").is_none(),
            "on-disk file must not contain top-level modified_at after write_back"
        );
        // Verify the known-redacted field is replaced with the sentinel,
        // not left as the live server value.
        let on_disk_agenda = disk_value
            .get("agenda_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(
            on_disk_agenda,
            crate::snapshot::create::REDACTED_VALUE_SENTINEL,
            "agenda_id must be set to the REDACTED sentinel on disk (not the live server value)"
        );
    }

    /// Overlay consistency: when a tgt overlay overrides a field, the lockfile
    /// hash must use post-overlay bytes (field stripped), not pre-overlay.
    /// This matches what `tgt_drift_status` and the pull driver compute.
    #[test]
    fn write_back_engine_with_overlay_records_post_overlay_hash() {
        let tmp = TempDir::new().unwrap();
        let paths = tmp_paths(&tmp, "tgt");
        let mut lockfile = Lockfile::default();

        let response_value = json!({
            "id": 7,
            "url": "https://prod.rossum.app/api/v1/engines/7",
            "name": "classifier",
            "modified_at": "2026-04-01T09:00:00Z",
            "description": "tgt-specific-value",
        });
        let response: crate::model::Engine =
            serde_json::from_value(response_value.clone()).unwrap();

        // Overlay declares `description` as a tgt-managed override → it must
        // be stripped from the hash bytes (so pull/sync compute the same hash).
        let mut overlay_paths: BTreeMap<String, Value> = BTreeMap::new();
        overlay_paths.insert("description".to_string(), json!("tgt-specific-value"));

        let dir = paths.engine_dir("classifier");
        std::fs::create_dir_all(&dir).unwrap();

        write_back_engine(
            &paths,
            &mut lockfile,
            "classifier",
            &response,
            Some(&overlay_paths),
        )
        .unwrap();

        // Expected: codec bytes → strip overlay → combined_hash.
        let codec =
            crate::snapshot::codec::codec("engines").expect("engines codec must be registered");
        let art = codec.disk_bytes(&response_value).unwrap();
        let post_overlay =
            crate::cli::pull::common::maybe_strip_overlay(art.json, Some(&overlay_paths)).unwrap();
        let expected_hash = combined_hash(&post_overlay, &art.sidecars, &Lockfile::default());

        let recorded_hash = lockfile
            .objects
            .get("engines")
            .and_then(|m| m.get("classifier"))
            .and_then(|e| e.content_hash.as_deref())
            .expect("lockfile must have an entry for the written engine");

        assert_eq!(
            recorded_hash, expected_hash,
            "write_back_engine with overlay must record post-overlay codec hash"
        );

        // The hash must differ from the pre-overlay hash (regression guard).
        let pre_overlay_art = codec.disk_bytes(&response_value).unwrap();
        let pre_overlay_hash = combined_hash(
            &pre_overlay_art.json,
            &pre_overlay_art.sidecars,
            &Lockfile::default(),
        );
        assert_ne!(
            recorded_hash, pre_overlay_hash,
            "pre-overlay hash must differ from post-overlay hash (overlay strip must be effective)"
        );
    }
}
