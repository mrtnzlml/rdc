//! Push driver for the `organization` kind — the only write rdc makes to an
//! organization, and the narrowest one in the codebase.
//!
//! Scope is a single subtree, `settings`. Everything else on the object is
//! read-only at the API or per-env state rdc must not own (`ui_settings` holds
//! branding and the org's applied feature flags; `metadata` is free-form).
//!
//! The body is always a COMPLETE `settings` object, never a fragment: a partial
//! `settings` PATCH replaces the stored object, so sending just one table
//! silently drops its siblings (verified against a live env).
//!
//! No create, no delete: one organization exists per env, made outside rdc.

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;

use crate::api::RossumClient;
use crate::log::{Action, Log};
use crate::paths::Paths;
use crate::state::Lockfile;

/// Top-level keys rdc sends. Everything else in the file is informational.
const MANAGED: &[&str] = &["settings"];

pub async fn push(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    path: &Path,
    progress: &Arc<Log>,
    env: &str,
) -> Result<(usize, usize)> {
    // The org's id comes from the lockfile: rdc never creates one, so a missing
    // entry means this project has not pulled the org yet. The pull half of the
    // same sync records it; nothing to push this cycle.
    let Some(id) = lockfile
        .objects
        .get("organization")
        .and_then(|m| m.get("self"))
        .map(|e| e.id)
    else {
        progress.event(
            Action::Warn,
            "organization not in the lockfile yet — settings not pushed (pull first)",
        );
        return Ok((0, 1));
    };

    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let local: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;

    // An absent `settings` is NOT the same as an empty one. rdc cannot tell
    // "this project does not manage org settings" from "clear them", and
    // guessing the second wipes the remote — so it does neither. Clearing is
    // written explicitly, as `"settings": {}`.
    let Some(settings) = local.get("settings") else {
        progress.event(
            Action::Warn,
            &format!(
                "{}: no `settings` key — nothing pushed. rdc manages only `settings` on an \
                 organization; write `\"settings\": {{}}` to clear it",
                path.display()
            ),
        );
        return Ok((0, 1));
    };

    // Local edits rdc cannot push would be discarded by the write-back below,
    // which rewrites the file from the server's response. Say so first.
    let unmanaged: Vec<String> = local
        .as_object()
        .map(|o| {
            o.keys()
                .filter(|k| !MANAGED.contains(&k.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    // Whether `settings` itself actually changed since the last synced base —
    // NOT whether the whole file changed. The scanner's gate
    // (`push::scan::scan_organization`) hashes the entire file, so an edit
    // confined to an unmanaged field (e.g. the org's top-level `ui_settings`,
    // distinct from the nested `settings.ui_settings` rdc manages) still lands
    // here with `settings` present but byte-identical to the base. Pushing
    // that would PATCH an unchanged `settings` and then the write-back below
    // would overwrite the local file with the server's response — silently
    // discarding the unmanaged edit the push could never carry in the first
    // place. Compare against the base cache (the last canonical bytes
    // `pull`/`push` wrote) rather than re-fetching the remote, so this costs
    // no extra round trip. Unreadable/missing/unparseable base cache (e.g. a
    // push before any prior pull recorded one) is treated as "can't prove
    // it's a no-op" and falls through to pushing — the safe default here is
    // to send it, not to silently swallow a real change.
    let base_settings = crate::state::base_cache::read(paths, path)
        .ok()
        .flatten()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("settings").cloned());
    if base_settings.as_ref() == Some(settings) {
        progress.event(
            Action::Warn,
            &format!(
                "{}: `settings` unchanged; not pushing ({} not managed by rdc)",
                path.display(),
                if unmanaged.is_empty() {
                    "nothing else changed".to_string()
                } else {
                    unmanaged.join(", ")
                }
            ),
        );
        return Ok((0, 1));
    }

    if !unmanaged.is_empty() {
        progress.event(
            Action::Info,
            &format!(
                "organization: only `settings` is pushed; {} stay as the env has them",
                unmanaged.join(", ")
            ),
        );
    }

    let body = serde_json::json!({ "settings": settings });
    let updated = client
        .update_organization(id, &body, Some(progress.clone()))
        .await
        .with_context(|| format!("patching organization settings for env '{env}'"))?;

    // Canonical write-back: the same bytes a pull would produce, so the next
    // sync sees `Clean` (this is also what normalizes `width: 120` to the
    // server's `120.0`).
    let value = serde_json::to_value(&updated).context("serializing patched organization")?;
    let art = crate::snapshot::codec::codec("organization")
        .expect("organization codec must exist")
        .disk_bytes(&value)
        .context("serializing organization")?;
    let json = crate::cli::pull::common::portabilize_proposed(&art.json, lockfile);
    let hash = crate::snapshot::codec::combined_hash(&json, &art.sidecars, lockfile);
    crate::state::base_cache::write_disk_and_cache(paths, path, &json)?;
    crate::cli::pull::common::record_object(
        lockfile,
        "organization",
        "self",
        updated.id,
        updated.modified_at().map(|s| s.to_string()),
        updated.modified_by().map(|s| s.to_string()),
        Some(hash),
    );
    progress.event(Action::Patch, "organization settings");
    Ok((1, 0))
}
