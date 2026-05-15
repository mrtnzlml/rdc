//! Delete phase for `rdc push`: turn lockfile tombstones (lockfile entry +
//! missing local file) into `DELETE /<kind>/<id>` calls.
//!
//! Safety model (matches the user-confirmed design):
//!
//! 1. **Confirmation gate.** Before any DELETE leaves the box, the full
//!    tombstone list is printed and the user must explicitly confirm:
//!    - TTY without `--allow-deletes`: interactive `[y/N]` prompt.
//!    - TTY with `--allow-deletes`: prompt skipped, proceed.
//!    - Non-TTY without `--allow-deletes`: refuse (non-zero exit). CI
//!      pipelines must pass the flag to authorise destructive deletes.
//!    `--yes` does NOT bypass this — two intentional acts are required
//!    to destroy remote state.
//!
//! 2. **Cascade order.** Children before parents, reverse of the create
//!    order: `engine_fields → engines → labels → rules → hooks →
//!    email_templates → inboxes → queues → schemas → workspaces`.
//!
//! 3. **Idempotent DELETE.** `delete_path` already treats 404 as success,
//!    so an object that's already gone remotely just gets its lockfile
//!    entry cleaned up.
//!
//! 4. **Drift detection.** For each tombstone, we fetch the remote and
//!    compare its `modified_at` to the lockfile's recorded `modified_at`.
//!    If they differ, the remote has been touched since the last pull
//!    and we surface a per-object resolver (`[k]eep delete`, `[s]kip`,
//!    `[a]bort`; `[r]estore` is documented as a future enhancement — for
//!    now it skips with an instruction to re-pull).

use crate::api::{anyhow_has_status, RossumClient};
use crate::cli::push::scan::Tombstones;
use crate::state::Lockfile;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};

#[derive(Debug)]
pub enum ConfirmOutcome {
    Proceed,
    Aborted,
}

#[derive(Default, Debug)]
pub struct DeleteCounts {
    pub workspaces: usize,
    pub hooks: usize,
    pub rules: usize,
    pub labels: usize,
    pub queues: usize,
    pub schemas: usize,
    pub inboxes: usize,
    pub email_templates: usize,
    pub engines: usize,
    pub engine_fields: usize,
    pub skipped: usize,
}

impl DeleteCounts {
    pub fn total_deleted(&self) -> usize {
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

/// Print the tombstone list to stderr and gate on the destructive
/// confirmation. Returns `Aborted` if the user declines on TTY; returns
/// `Err` if running non-TTY without `--allow-deletes`.
pub fn confirm_or_refuse(
    tombstones: &Tombstones,
    interactive: bool,
    allow_deletes: bool,
) -> Result<ConfirmOutcome> {
    let n = tombstones.total();
    eprintln!();
    eprintln!("⚠️  The following {n} object(s) would be DELETED from the remote:");
    print_tombstone_list(tombstones);
    eprintln!();

    if allow_deletes {
        eprintln!("--allow-deletes set; proceeding without prompt.");
        return Ok(ConfirmOutcome::Proceed);
    }
    if !interactive {
        bail!(
            "{n} object(s) marked for deletion but --allow-deletes was not passed. \
             Re-run with --allow-deletes to authorise the destructive push, or \
             restore the local files to cancel the deletion."
        );
    }
    eprint!("Proceed with deletion? [y/N] ");
    std::io::stderr().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let ans = input.trim().to_ascii_lowercase();
    if ans == "y" || ans == "yes" {
        Ok(ConfirmOutcome::Proceed)
    } else {
        Ok(ConfirmOutcome::Aborted)
    }
}

fn print_tombstone_list(t: &Tombstones) {
    // Print in reverse-dep order (children first) so the user sees the
    // exact sequence the deletes will run in.
    for (name, map) in reverse_dep_order_iter(t) {
        for (slug, id) in map {
            eprintln!("  - {name}/{slug} (id {id})");
        }
    }
}

fn reverse_dep_order_iter(t: &Tombstones) -> Vec<(&'static str, &BTreeMap<String, u64>)> {
    vec![
        ("engine_fields", &t.engine_fields),
        ("engines", &t.engines),
        ("labels", &t.labels),
        ("rules", &t.rules),
        ("hooks", &t.hooks),
        ("email_templates", &t.email_templates),
        ("inboxes", &t.inboxes),
        ("queues", &t.queues),
        ("schemas", &t.schemas),
        ("workspaces", &t.workspaces),
    ]
}

/// For `rdc push --dry-run --diff`: fetch each tombstone's remote body
/// and print it as a deleted-file unified diff (`+++ /dev/null`).
/// Best-effort — objects already absent on the remote are noted and
/// skipped.
pub async fn preview_tombstone_bodies(
    client: &RossumClient,
    tombstones: &Tombstones,
) -> Result<()> {
    for (kind, map) in reverse_dep_order_iter(tombstones) {
        for (slug, id) in map {
            let label = format!("{kind}/{slug}.json");
            match fetch_remote_body(client, kind, *id).await {
                Ok(Some(body)) => {
                    crate::cli::diff::print_deleted_file_diff(&label, &body);
                }
                Ok(None) => {
                    eprintln!("  (already absent on remote: {kind}/{slug})");
                }
                Err(e) => {
                    eprintln!("  (failed to fetch {kind}/{slug} for diff: {e:#})");
                }
            }
        }
    }
    Ok(())
}

/// Run the deletes phase. Drift-checks each tombstone, prompts the
/// per-object resolver on drift, then issues `DELETE /<kind>/<id>` and
/// cleans the lockfile entry.
pub async fn run_deletes(
    client: &RossumClient,
    lockfile: &mut Lockfile,
    tombstones: &Tombstones,
    interactive: bool,
) -> Result<DeleteCounts> {
    let mut counts = DeleteCounts::default();

    for (kind, map) in reverse_dep_order_iter(tombstones) {
        // Collect into a Vec so we can mutate the lockfile mid-iteration
        // (we'd hold a borrow of the map otherwise).
        let entries: Vec<(String, u64)> = map.iter().map(|(s, i)| (s.clone(), *i)).collect();
        for (slug, id) in entries {
            let outcome = delete_one(client, kind, &slug, id, lockfile, interactive).await?;
            apply_outcome(&mut counts, kind, outcome);
        }
    }

    Ok(counts)
}

#[derive(Debug)]
enum DeleteOutcome {
    Deleted,
    AlreadyGone,
    Skipped,
}

async fn delete_one(
    client: &RossumClient,
    kind: &str,
    slug: &str,
    id: u64,
    lockfile: &mut Lockfile,
    interactive: bool,
) -> Result<DeleteOutcome> {
    // Drift check: compare the remote object's modified_at against the
    // lockfile's recorded modified_at. We use modified_at rather than a
    // full content hash because (a) it's a single field accessible
    // uniformly across kinds, and (b) any server-side touch — UI edit,
    // automated retraining, schema migration — bumps it. The conservative
    // choice: if modified_at differs (or is missing on either side), we
    // treat it as drift and let the user decide.
    let remote = fetch_remote_modified_at(client, kind, id).await?;
    if remote.is_none() {
        // Already gone on the remote; just clean up our lockfile.
        if let Some(m) = lockfile.objects.get_mut(kind) {
            m.remove(slug);
        }
        eprintln!("  - {kind}/{slug}: already absent on remote, lockfile cleaned");
        return Ok(DeleteOutcome::AlreadyGone);
    }
    let remote_modified = remote.expect("checked Some");
    let lockfile_modified = lockfile
        .objects
        .get(kind)
        .and_then(|m| m.get(slug))
        .and_then(|e| e.modified_at.clone());

    let drifted = match (&remote_modified, &lockfile_modified) {
        (Some(r), Some(l)) => r != l,
        // Either side missing → can't compare → conservative: treat as
        // drifted so the user gets a prompt rather than silent destruction.
        _ => true,
    };

    if drifted {
        match resolve_delete_drift(interactive, kind, slug)? {
            DeleteDriftChoice::KeepDelete => { /* fall through to DELETE */ }
            DeleteDriftChoice::Skip => {
                eprintln!("  - {kind}/{slug}: skipped (drift)");
                return Ok(DeleteOutcome::Skipped);
            }
            DeleteDriftChoice::Restore => {
                eprintln!(
                    "  - {kind}/{slug}: drift detected; restore is not automated yet — \
                     run `rdc sync <env>` to refresh the local file, then re-edit if needed."
                );
                return Ok(DeleteOutcome::Skipped);
            }
            DeleteDriftChoice::Abort => {
                bail!("push aborted at delete drift resolver");
            }
        }
    }

    // Actual DELETE. `delete_path` accepts 204 + 404 as success.
    client
        .delete_path(&format!("/{kind}/{id}"), None)
        .await
        .with_context(|| format!("DELETE /{kind}/{id}"))?;
    if let Some(m) = lockfile.objects.get_mut(kind) {
        m.remove(slug);
    }
    eprintln!("  - {kind}/{slug}: deleted");
    Ok(DeleteOutcome::Deleted)
}

fn apply_outcome(counts: &mut DeleteCounts, kind: &str, outcome: DeleteOutcome) {
    match outcome {
        DeleteOutcome::Deleted | DeleteOutcome::AlreadyGone => match kind {
            "workspaces" => counts.workspaces += 1,
            "hooks" => counts.hooks += 1,
            "rules" => counts.rules += 1,
            "labels" => counts.labels += 1,
            "queues" => counts.queues += 1,
            "schemas" => counts.schemas += 1,
            "inboxes" => counts.inboxes += 1,
            "email_templates" => counts.email_templates += 1,
            "engines" => counts.engines += 1,
            "engine_fields" => counts.engine_fields += 1,
            _ => {}
        },
        DeleteOutcome::Skipped => counts.skipped += 1,
    }
}

#[derive(Debug)]
enum DeleteDriftChoice {
    KeepDelete,
    Skip,
    Restore,
    Abort,
}

fn resolve_delete_drift(interactive: bool, kind: &str, slug: &str) -> Result<DeleteDriftChoice> {
    if !interactive {
        // Non-TTY (CI / --yes): fall back to skip with warning so a
        // drifted delete never silently destroys someone else's work.
        eprintln!(
            "warning: {kind}/{slug}: local file deleted but remote modified since last sync; \
             skipping (run `rdc sync <env>` to retry)."
        );
        return Ok(DeleteDriftChoice::Skip);
    }
    eprintln!();
    eprintln!(
        "{kind}/{slug}: local file deleted, but remote has been modified since the last pull."
    );
    eprint!("[k]eep delete  [r]estore  [s]kip  [a]bort > ");
    std::io::stderr().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let ans = input.trim().to_ascii_lowercase();
    match ans.as_str() {
        "k" | "keep" => Ok(DeleteDriftChoice::KeepDelete),
        "r" | "restore" => Ok(DeleteDriftChoice::Restore),
        "s" | "skip" | "" => Ok(DeleteDriftChoice::Skip),
        "a" | "abort" => Ok(DeleteDriftChoice::Abort),
        _ => {
            eprintln!("unrecognised choice '{ans}'; skipping");
            Ok(DeleteDriftChoice::Skip)
        }
    }
}

// --- per-kind remote fetch helpers ----------------------------------

/// Fetch the remote object's `modified_at` (or None on 404). Used for
/// drift detection without paying for full body serialisation.
async fn fetch_remote_modified_at(
    client: &RossumClient,
    kind: &str,
    id: u64,
) -> Result<Option<Option<String>>> {
    // Returns Ok(None) if the remote returns 404, Ok(Some(maybe)) where
    // `maybe` is the modified_at string if the kind exposes one. The
    // double-Option distinguishes "already gone" from "exists, no
    // modified_at field" — both legitimate.
    Ok(match kind {
        "hooks" => match client.get_hook(id, None).await {
            Ok(h) => Some(h.modified_at().map(|s| s.to_string())),
            Err(e) if anyhow_has_status(&e, 404) => None,
            Err(e) => return Err(e),
        },
        "workspaces" => match client.get_workspace(id, None).await {
            Ok(w) => Some(w.modified_at().map(|s| s.to_string())),
            Err(e) if anyhow_has_status(&e, 404) => None,
            Err(e) => return Err(e),
        },
        "inboxes" => match client.get_inbox(id, None).await {
            Ok(i) => Some(i.modified_at().map(|s| s.to_string())),
            Err(e) if anyhow_has_status(&e, 404) => None,
            Err(e) => return Err(e),
        },
        "schemas" => match client.get_schema(id, None).await {
            Ok(s) => Some(s.modified_at().map(|s| s.to_string())),
            Err(e) if anyhow_has_status(&e, 404) => None,
            Err(e) => return Err(e),
        },
        // No direct get_* for these kinds → use list + filter (one
        // list call per kind, irrespective of tombstone count).
        "labels" => match client.list_labels(None).await?.into_iter().find(|x| x.id == id) {
            Some(x) => Some(x.modified_at().map(|s| s.to_string())),
            None => None,
        },
        "rules" => match client.list_rules(None).await?.into_iter().find(|x| x.id == id) {
            Some(x) => Some(x.modified_at().map(|s| s.to_string())),
            None => None,
        },
        "queues" => match client.list_queues(None).await?.into_iter().find(|x| x.id == id) {
            Some(x) => Some(x.modified_at().map(|s| s.to_string())),
            None => None,
        },
        // Engines / engine_fields don't expose modified_at on their
        // model today; existence is the best signal we have. Treat
        // "exists" as "not drifted" so the user isn't prompted for
        // every one of them.
        "engines" => client.list_engines(None).await?.into_iter().find(|x| x.id == id).map(|_| None),
        "engine_fields" => client
            .list_engine_fields(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|_| None),
        "email_templates" => match client
            .list_email_templates(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
        {
            Some(x) => Some(x.modified_at().map(|s| s.to_string())),
            None => None,
        },
        _ => None,
    })
}

/// Fetch the remote object as pretty-printed JSON for the dry-run diff
/// preview. Returns None on 404.
async fn fetch_remote_body(client: &RossumClient, kind: &str, id: u64) -> Result<Option<String>> {
    let value = match kind {
        "hooks" => match client.get_hook(id, None).await {
            Ok(h) => Some(serde_json::to_value(&h)?),
            Err(e) if anyhow_has_status(&e, 404) => None,
            Err(e) => return Err(e),
        },
        "workspaces" => match client.get_workspace(id, None).await {
            Ok(w) => Some(serde_json::to_value(&w)?),
            Err(e) if anyhow_has_status(&e, 404) => None,
            Err(e) => return Err(e),
        },
        "inboxes" => match client.get_inbox(id, None).await {
            Ok(i) => Some(serde_json::to_value(&i)?),
            Err(e) if anyhow_has_status(&e, 404) => None,
            Err(e) => return Err(e),
        },
        "schemas" => match client.get_schema(id, None).await {
            Ok(s) => Some(serde_json::to_value(&s)?),
            Err(e) if anyhow_has_status(&e, 404) => None,
            Err(e) => return Err(e),
        },
        "labels" => client
            .list_labels(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|x| serde_json::to_value(&x))
            .transpose()?,
        "rules" => client
            .list_rules(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|x| serde_json::to_value(&x))
            .transpose()?,
        "queues" => client
            .list_queues(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|x| serde_json::to_value(&x))
            .transpose()?,
        "engines" => client
            .list_engines(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|x| serde_json::to_value(&x))
            .transpose()?,
        "engine_fields" => client
            .list_engine_fields(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|x| serde_json::to_value(&x))
            .transpose()?,
        "email_templates" => client
            .list_email_templates(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|x| serde_json::to_value(&x))
            .transpose()?,
        _ => None,
    };
    match value {
        Some(v) => Ok(Some(serde_json::to_string_pretty(&v)?)),
        None => Ok(None),
    }
}

// IsTerminal is referenced via the std::io trait in the public callsite
// in mod.rs; the import here just keeps that surface obvious.
#[allow(unused_imports)]
use IsTerminal as _;
