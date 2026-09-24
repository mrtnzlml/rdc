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
    // written explicitly, as `"settings": {}`. A `settings: null` is treated
    // exactly the same as an absent key — it is not a probed API shape (only
    // `{}` is), and `migrate` can produce it (see `reconcile_target_identity`),
    // so guessing there too would risk the same wipe.
    let settings = local.get("settings").filter(|v| !v.is_null());
    let Some(settings) = settings else {
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

    // Resolve any `rdc://` refs before the wire, matching every other push
    // driver (e.g. `labels.rs`, `rules.rs`, `queues.rs`). Not reachable with
    // today's column shape — a document-list column never carries a portable
    // ref — but `portabilize_value` walks every string leaf regardless of
    // field name, and `migrate` actively rewrites refs inside a promoted
    // body, so the organization IS a kind that could acquire an `rdc://…`
    // string offline. Skipping this would PATCH that literal string into a
    // remote setting.
    let mut settings_resolved = settings.clone();
    crate::snapshot::refs::resolve_value(&mut settings_resolved, lockfile);
    let body = serde_json::json!({ "settings": settings_resolved });
    let updated = client
        .update_organization(id, &body, Some(progress.clone()))
        .await
        .with_context(|| format!("patching organization settings for env '{env}'"))?;
    // No remote read precedes this PATCH; the base is the last value the
    // server returned, which is what "kept its old value" compares against.
    if let Some(base) = &base_settings {
        crate::cli::push::warn_ignored(
            progress,
            "organization",
            &body,
            &serde_json::json!({ "settings": base }),
            &updated,
        );
    }

    // Write back ONLY `settings`, merged into the body already on disk.
    //
    // The obvious thing — write the PATCH response — is wrong, and it took a
    // live organization to show it: the response is NOT shaped like a `GET`.
    // It carries `rir_key`, which `GET /organizations/{id}` omits entirely,
    // and it returns `users` in a different order. Writing it wholesale put a
    // field on disk that no pull ever produces, so the very next sync
    // classified the org `RemoteEdit` and pulled it back to correct itself —
    // one phantom "1 changed" cycle after every settings push. Every
    // mock-based test missed it, because a mock naturally answers GET and
    // PATCH with the same body; `sync_organization_write_back_keeps_the_shape_
    // a_pull_would_produce` reproduces the asymmetry on purpose.
    //
    // Taking `settings` FROM the response is deliberate and is the whole
    // reason to look at it at all: that is the subtree rdc manages, and the
    // server normalizes it (`width: 140` comes back `140.0`,
    // `annotation_list_table: {}` comes back `columns: []`), so the
    // normalized form is what belongs on disk. Every other field keeps the
    // value `pull` wrote, which is by definition the shape a pull produces.
    //
    // This also removes the need for the divergence notice that used to live
    // here. It compared `local` against the response and warned that the
    // differing keys "will be overwritten by the env's value" — but with the
    // write-back scoped to `settings`, nothing outside `settings` is ever
    // overwritten, so there was nothing truthful left to warn about. On a real
    // org it fired on EVERY push (naming `rir_key`, `users`, `workspaces`),
    // which is worse than silence: a warning that cries wolf on the happy
    // path teaches people to ignore it. An unmanaged local edit is still not
    // pushed — it simply stays on disk until the pull half reverts it, the
    // same as any other locally-edited field rdc does not own.
    let updated_settings = serde_json::to_value(&updated)
        .context("serializing patched organization")?
        .get("settings")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let mut merged = local.clone();
    match merged.as_object_mut() {
        Some(obj) => {
            obj.insert("settings".to_string(), updated_settings);
        }
        // `local` parsed as a non-object cannot happen: the `settings` lookup
        // above already required an object. Bail rather than write a body
        // shaped like nothing pull would produce.
        None => anyhow::bail!("{}: expected a JSON object", path.display()),
    }
    let art = crate::snapshot::codec::codec("organization")
        .expect("organization codec must exist")
        .disk_bytes(&merged)
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
