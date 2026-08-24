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
    // same sync records it; nothing to push this cycle. `lockfile_hash` backs
    // the base-cache trust check below.
    let Some((id, lockfile_hash)) = lockfile
        .objects
        .get("organization")
        .and_then(|m| m.get("self"))
        .map(|e| (e.id, e.content_hash.clone()))
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
    // no extra round trip.
    //
    // The cache is trustworthy for this comparison ONLY when its hash still
    // matches the lockfile's recorded `content_hash`. A conflict auto-merge
    // that keeps a local `settings` edit against a disjoint remote change
    // (`execute::try_auto_merge`) deliberately breaks that equality: it
    // writes the MERGED bytes into both the env file and the base-cache
    // mirror (so a future 3-way merge sees the reconciled state), but pins
    // the lockfile to the pre-merge remote hash on purpose, so the next
    // classify sees `LocalEdit` and this driver gets a same-cycle chance to
    // push it (see the `promoted_to_push` handling in `cli::sync::execute`).
    // In that window the cache's `settings` already equals local's — using
    // it here would read back as "unchanged" and silently drop an edit that
    // has in fact never reached the remote. Unreadable/missing/unparseable/
    // stale-relative-to-the-lockfile base cache is therefore treated the
    // same as "can't prove it's a no-op" and falls through to pushing — the
    // safe default is to send it, not to silently swallow a real change.
    let base_settings = crate::state::base_cache::read(paths, path)
        .ok()
        .flatten()
        .filter(|b| {
            lockfile_hash.as_deref()
                == Some(
                    crate::snapshot::codec::combined_hash(b, &[], &crate::state::Lockfile::default())
                        .as_str(),
                )
        })
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

    let body = serde_json::json!({ "settings": settings });
    let updated = client
        .update_organization(id, &body, Some(progress.clone()))
        .await
        .with_context(|| format!("patching organization settings for env '{env}'"))?;

    // Compare local vs the server's response, ignoring `settings` (the
    // subtree rdc actually manages) and the hidden stamps
    // (`modified_at`/`modified_by` — stripped from disk by the codec, so
    // they'd always look "locally absent" and falsely diverge; a stamp bump
    // alone must never read as a discarded edit). Any other top-level key
    // that differs between the two is about to be silently overwritten by
    // the canonical write-back below — name exactly those keys, not the
    // full `unmanaged` list, so the warning is accurate rather than noise
    // on every push.
    let value = serde_json::to_value(&updated).context("serializing patched organization")?;
    let diverged: Vec<String> = {
        const IGNORED: &[&str] = &["settings", "modified_at", "modified_by"];
        let local_obj = local.as_object();
        let remote_obj = value.as_object();
        let mut keys: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        if let Some(o) = local_obj {
            keys.extend(o.keys().map(String::as_str));
        }
        if let Some(o) = remote_obj {
            keys.extend(o.keys().map(String::as_str));
        }
        keys.into_iter()
            .filter(|k| !IGNORED.contains(k))
            .filter(|k| local_obj.and_then(|o| o.get(*k)) != remote_obj.and_then(|o| o.get(*k)))
            .map(str::to_string)
            .collect()
    };
    if !diverged.is_empty() {
        progress.event(
            Action::Warn,
            &format!(
                "organization: {} will be overwritten by the env's value (rdc only manages `settings`)",
                diverged.join(", ")
            ),
        );
    }

    // Canonical write-back: the same bytes a pull would produce, so the next
    // sync sees `Clean` (this is also what normalizes `width: 120` to the
    // server's `120.0`).
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
