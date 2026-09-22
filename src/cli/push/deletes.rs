//! Delete phase of `rdc sync`'s push: turn lockfile tombstones (lockfile
//! entry + missing local file) into `DELETE /<kind>/<id>` calls.
//!
//! Safety model (matches the user-confirmed design):
//!
//! 1. **Confirmation gate.** Before any DELETE leaves the box, the full
//!    tombstone list is printed and the user must explicitly confirm:
//!    - TTY without `--allow-deletes`: interactive `[y/N]` prompt.
//!    - TTY with `--allow-deletes`: prompt skipped, proceed.
//!    - Non-TTY without `--allow-deletes`: refuse (non-zero exit). CI
//!      pipelines must pass the flag to authorise destructive deletes.
//!      `--yes` does NOT bypass this — two intentional acts are required
//!      to destroy remote state.
//!
//! 2. **Cascade order.** Children before parents, reverse of the create
//!    order: `engine_fields → engines → labels → saved_views → rules → hooks →
//!    email_templates → inboxes → queues → schemas → workspaces`. Nothing
//!    references a saved view, so its position among the leaves is free.
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

use crate::api::{RossumClient, anyhow_has_status};
use crate::cli::push::scan::Tombstones;
use crate::log::{Action, Log};

use crate::state::Lockfile;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::sync::Arc;

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
    pub saved_views: usize,
    pub queues: usize,
    pub schemas: usize,
    pub inboxes: usize,
    pub email_templates: usize,
    pub engines: usize,
    pub engine_fields: usize,
    pub skipped: usize,
    /// Per-object DELETEs the remote rejected (e.g. a `400` on a
    /// unique-type `email_template`, a `409`, etc.). These are skipped
    /// and tallied rather than aborting the batch — see `run_deletes`.
    pub failed: usize,
}

impl DeleteCounts {
    pub fn total_deleted(&self) -> usize {
        self.workspaces
            + self.hooks
            + self.rules
            + self.labels
            + self.saved_views
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
    progress: &Arc<Log>,
) -> Result<ConfirmOutcome> {
    use crate::cli::change_view::{ChangeRow, RowVerb, RowWidths, render_row};
    let n = tombstones.total();

    // Through `Log`, not `eprintln!`. This is the most destructive list rdc
    // prints and it used to bypass the renderer entirely, which meant an
    // embedder consuming `Log::for_sink` never saw a single line of it.
    //
    // Rows are in reverse-dependency order (children first), so the list is
    // also the exact sequence the deletes will run in.
    progress.event(
        Action::Delete,
        &format!("{n} object(s) would be DELETED from the remote"),
    );
    let mut rows: Vec<(&'static str, &str, u64)> = Vec::new();
    for (kind, map) in reverse_dep_order_iter(tombstones) {
        for (slug, id) in map {
            rows.push((kind, slug.as_str(), *id));
        }
    }
    let w = RowWidths::fit(rows.iter().map(|(k, s, _)| (*k, *s)));
    let mode = crate::cli::resolve::detect_color_mode();
    for (kind, slug, id) in &rows {
        let note = format!("id {id}");
        progress.row(&render_row(
            &ChangeRow {
                verb: RowVerb::Delete,
                kind,
                name: slug,
                // A tombstone is decided from the lockfile; rdc holds no body
                // to count lines against.
                added: None,
                removed: None,
                note: Some(&note),
            },
            w,
            mode,
        ));
    }

    if allow_deletes {
        progress.event(
            Action::Info,
            "--allow-deletes set; proceeding without prompt",
        );
        return Ok(ConfirmOutcome::Proceed);
    }
    if !interactive {
        bail!(
            "{n} object(s) marked for deletion but --allow-deletes was not passed. \
             Re-run with --allow-deletes to authorise the destructive push, or \
             restore the local files to cancel the deletion."
        );
    }
    // The question is written through the renderer rather than emitted as a
    // Log event: it must sit on the cursor's line for the answer to be typed
    // after it, which a timestamped event line cannot do. Under `Log::new`
    // the renderer's sink is stderr, so the terminal sees the same bytes.
    crate::cli::stdin_coord::announce(crate::cli::stdin_coord::Prompt {
        kind: crate::cli::stdin_coord::PromptKind::DeleteGate,
        question: "Proceed with deletion? [y/N] ".into(),
        keys: vec![
            crate::cli::stdin_coord::PromptKey::new('y', "delete them"),
            crate::cli::stdin_coord::PromptKey::new('n', "cancel"),
        ],
    });
    let mut q = progress.writer();
    write!(q, "Proceed with deletion? [y/N] ").ok();
    q.flush().ok();
    // Route via the stdin coordinator so this prompt cooperates with the
    // `rdc sync --watch` Enter-trigger reader instead of fighting it for
    // the terminal. Outside watch it reads stdin directly.
    let ans = crate::cli::stdin_coord::read_line_coordinated()?
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if ans == "y" || ans == "yes" {
        Ok(ConfirmOutcome::Proceed)
    } else {
        Ok(ConfirmOutcome::Aborted)
    }
}

fn reverse_dep_order_iter(t: &Tombstones) -> Vec<(&'static str, &BTreeMap<String, u64>)> {
    vec![
        ("engine_fields", &t.engine_fields),
        ("engines", &t.engines),
        ("labels", &t.labels),
        ("saved_views", &t.saved_views),
        ("rules", &t.rules),
        ("hooks", &t.hooks),
        ("email_templates", &t.email_templates),
        ("inboxes", &t.inboxes),
        ("queues", &t.queues),
        ("schemas", &t.schemas),
        ("workspaces", &t.workspaces),
    ]
}

/// Run the deletes phase. Drift-checks each tombstone, prompts the
/// per-object resolver on drift, then issues `DELETE /<kind>/<id>` and
/// cleans the lockfile entry.
///
/// **Skip-and-continue.** A per-object DELETE failure (any non-2xx the
/// remote returns — e.g. a `400 Cannot delete template with unique type:
/// rejection_default` on a server-auto-created `email_template`, or a
/// `409`) is *not* propagated. If we `?`-bailed on the first error the
/// whole reverse-dependency batch would abort, orphaning every sibling
/// and parent that was perfectly deletable. Instead we emit a warning,
/// tally it in `DeleteCounts::failed`, leave the lockfile entry intact
/// (so a later sync retries it), and CONTINUE the loop. `run_deletes`
/// still returns `Ok` so the surrounding sync completes and every
/// deletable object actually gets deleted; the failures surface as
/// warnings in the run summary, never as a hard abort.
pub async fn run_deletes(
    client: &RossumClient,
    lockfile: &mut Lockfile,
    tombstones: &Tombstones,
    interactive: bool,
    progress: &Arc<Log>,
) -> Result<DeleteCounts> {
    let mut counts = DeleteCounts::default();

    for (kind, map) in reverse_dep_order_iter(tombstones) {
        // Collect into a Vec so we can mutate the lockfile mid-iteration
        // (we'd hold a borrow of the map otherwise).
        let entries: Vec<(String, u64)> = map.iter().map(|(s, i)| (s.clone(), *i)).collect();
        for (slug, id) in entries {
            match delete_one(client, kind, &slug, id, lockfile, interactive, progress).await {
                Ok(outcome) => apply_outcome(&mut counts, kind, outcome),
                Err(e) => {
                    // Skip-and-continue: the remote refused this DELETE.
                    // Warn, tally, leave the lockfile entry for retry, and
                    // keep going so siblings + parents still get deleted.
                    counts.failed += 1;
                    progress.event(
                        Action::Warn,
                        &format!("{kind}/{slug} delete failed (skipped): {e:#}"),
                    );
                }
            }
        }
    }

    if counts.failed > 0 {
        progress.event(
            Action::Warn,
            &format!(
                "{} object(s) could not be deleted and were skipped; they remain on the \
                 remote and in the lockfile. Re-run `rdc sync <env> --allow-deletes` to retry.",
                counts.failed
            ),
        );
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
    progress: &Arc<Log>,
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
        progress.event(
            Action::Skip,
            &format!("{kind}/{slug} (remote id {id} missing)"),
        );
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
        // Both sides agree the kind has no `modified_at` field on the
        // wire (e.g. labels). With no timestamp to compare, there's no
        // signal of drift — treat as clean and let the DELETE through.
        (None, None) => false,
        // One side has a timestamp the other doesn't — that's a real
        // discrepancy (e.g. the lockfile pre-dates a server-side schema
        // change). Conservative: treat as drift so the user gets a
        // prompt rather than silent destruction.
        _ => true,
    };

    if drifted {
        match resolve_delete_drift(progress, interactive, kind, slug)? {
            DeleteDriftChoice::KeepDelete => { /* fall through to DELETE */ }
            DeleteDriftChoice::Skip => {
                progress.event(Action::Skip, &format!("{kind}/{slug} (drift)"));
                return Ok(DeleteOutcome::Skipped);
            }
            DeleteDriftChoice::Restore => {
                progress.event(
                    Action::Skip,
                    &format!("{kind}/{slug} (drift; run `rdc sync` to restore)"),
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
    progress.event(Action::Delete, &format!("{kind}/{slug}"));
    Ok(DeleteOutcome::Deleted)
}

fn apply_outcome(counts: &mut DeleteCounts, kind: &str, outcome: DeleteOutcome) {
    match outcome {
        DeleteOutcome::Deleted | DeleteOutcome::AlreadyGone => match kind {
            "workspaces" => counts.workspaces += 1,
            "hooks" => counts.hooks += 1,
            "rules" => counts.rules += 1,
            "labels" => counts.labels += 1,
            "saved_views" => counts.saved_views += 1,
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

fn resolve_delete_drift(
    progress: &Arc<Log>,
    interactive: bool,
    kind: &str,
    slug: &str,
) -> Result<DeleteDriftChoice> {
    if !interactive {
        // Non-TTY (CI / --yes): fall back to skip with warning so a
        // drifted delete never silently destroys someone else's work.
        progress.event(
            Action::Warn,
            &format!(
                "{kind}/{slug}: local file deleted but remote modified since last sync; \
                 skipping (run `rdc sync <env>` to retry)."
            ),
        );
        return Ok(DeleteDriftChoice::Skip);
    }
    crate::cli::stdin_coord::announce(crate::cli::stdin_coord::Prompt {
        kind: crate::cli::stdin_coord::PromptKind::DeleteDrift,
        question: "[k]eep delete  [r]estore  [s]kip  [a]bort > ".into(),
        keys: vec![
            crate::cli::stdin_coord::PromptKey::new('k', "keep delete"),
            crate::cli::stdin_coord::PromptKey::new('r', "restore"),
            crate::cli::stdin_coord::PromptKey::new('s', "skip"),
            crate::cli::stdin_coord::PromptKey::new('a', "abort"),
        ],
    });
    let mut q = progress.writer();
    writeln!(q).ok();
    writeln!(
        q,
        "{kind}/{slug}: local file deleted, but remote has been modified since the last pull."
    )
    .ok();
    write!(q, "[k]eep delete  [r]estore  [s]kip  [a]bort > ").ok();
    q.flush().ok();
    let ans = crate::cli::stdin_coord::read_line_coordinated()?
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match ans.as_str() {
        "k" | "keep" => Ok(DeleteDriftChoice::KeepDelete),
        "r" | "restore" => Ok(DeleteDriftChoice::Restore),
        "s" | "skip" | "" => Ok(DeleteDriftChoice::Skip),
        "a" | "abort" => Ok(DeleteDriftChoice::Abort),
        other => {
            progress.event(Action::Warn, &format!("unrecognised choice '{other}'; skipping"));
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
        "labels" => client
            .list_labels(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|x| x.modified_at().map(|s| s.to_string())),
        "saved_views" => client
            .list_saved_views(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|x| x.modified_at().map(|s| s.to_string())),
        "rules" => client
            .list_rules(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|x| x.modified_at().map(|s| s.to_string())),
        "queues" => client
            .list_queues(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|x| x.modified_at().map(|s| s.to_string())),
        // Engines / engine_fields don't expose modified_at on their
        // model today; existence is the best signal we have. Treat
        // "exists" as "not drifted" so the user isn't prompted for
        // every one of them.
        "engines" => client
            .list_engines(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|_| None),
        "engine_fields" => client
            .list_engine_fields(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|_| None),
        "email_templates" => client
            .list_email_templates(None)
            .await?
            .into_iter()
            .find(|x| x.id == id)
            .map(|x| x.modified_at().map(|s| s.to_string())),
        _ => None,
    })
}


// IsTerminal is referenced via the std::io trait in the public callsite
// in mod.rs; the import here just keeps that surface obvious.
#[allow(unused_imports)]
use IsTerminal as _;

#[cfg(test)]
mod tests {
    /// In-memory `Log` sink, standing in for an embedder (the desktop app
    /// consumes rdc through `Log::for_sink`).
    #[derive(Clone, Default)]
    struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Buf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// The regression: this gate printed with `eprintln!`, so an embedder
    /// consuming `Log::for_sink` received not one line of the most
    /// destructive list rdc prints.
    #[test]
    fn delete_gate_writes_the_object_list_into_the_log_sink() {
        let buf = Buf::default();
        let log = crate::log::Log::for_sink(
            crate::cli::resolve::ColorMode::Plain,
            Box::new(buf.clone()),
        );
        let mut t = Tombstones::default();
        t.hooks.insert("legacy-export".to_string(), 9137);
        t.engines.insert("custom-eu".to_string(), 4410);

        // `allow_deletes` returns before the prompt, so this never reads stdin.
        let out = confirm_or_refuse(&t, false, true, &log).unwrap();
        assert!(matches!(out, ConfirmOutcome::Proceed));

        let text = buf.text();
        assert!(
            text.contains("2 object(s) would be DELETED from the remote"),
            "header missing from the sink: {text:?}"
        );
        for needle in ["hooks", "legacy-export", "id 9137", "engines", "custom-eu", "id 4410"] {
            assert!(text.contains(needle), "{needle:?} missing from the sink: {text:?}");
        }
        // Reverse-dependency order: engines (a child) before hooks, which is
        // the order the DELETEs actually run in.
        assert!(
            text.find("custom-eu").unwrap() < text.find("legacy-export").unwrap(),
            "rows must be in reverse-dependency order: {text:?}"
        );
        assert!(!text.contains('\u{1b}'), "Plain mode leaked SGR: {text:?}");
    }

    use super::*;

    /// `apply_outcome` has a `_ => {}` catch-all, so a deletable kind missing an
    /// arm would delete remotely and then not be counted — the summary would
    /// under-report a destructive action.
    #[test]
    fn every_deletable_kind_is_counted_by_apply_outcome() {
        for kind in crate::kinds::DELETABLE {
            let mut counts = DeleteCounts::default();
            apply_outcome(&mut counts, kind, DeleteOutcome::Deleted);
            assert_eq!(
                counts.total_deleted(),
                1,
                "apply_outcome did not count a delete for '{kind}'",
            );
        }
    }

    /// `AlreadyGone` counts as deleted (the remote object is gone either way);
    /// `Skipped` must not.
    #[test]
    fn apply_outcome_counts_already_gone_but_not_skipped() {
        let mut counts = DeleteCounts::default();
        apply_outcome(&mut counts, "labels", DeleteOutcome::AlreadyGone);
        assert_eq!(counts.total_deleted(), 1);

        let mut counts = DeleteCounts::default();
        apply_outcome(&mut counts, "labels", DeleteOutcome::Skipped);
        assert_eq!(counts.total_deleted(), 0);
    }

    /// An organization has no delete path at all, so it must never be counted.
    #[test]
    fn apply_outcome_does_not_count_an_organization() {
        let mut counts = DeleteCounts::default();
        apply_outcome(&mut counts, "organization", DeleteOutcome::Deleted);
        assert_eq!(counts.total_deleted(), 0);
    }

    /// Records every `Prompt` handed to it and answers with a canned string,
    /// standing in for a non-terminal consumer (the desktop route) that
    /// answers a gate without ever touching a real terminal.
    struct Recorder {
        answer: String,
        seen: std::sync::Mutex<Vec<crate::cli::stdin_coord::Prompt>>,
    }
    impl crate::cli::stdin_coord::PromptRoute for Recorder {
        fn ask(&self, prompt: &crate::cli::stdin_coord::Prompt) -> Option<String> {
            self.seen.lock().unwrap().push(prompt.clone());
            Some(self.answer.clone())
        }
    }

    /// Genuine coverage for the object-delete gate's `announce`, replacing
    /// the tautological capture test Task 3 removed (it never called
    /// `confirm_or_refuse` at all). Drives the gate for real through an
    /// installed route and asserts both halves: the question text still
    /// reaches the log sink, and the announced `Prompt` carries the `kind`
    /// and `keys` a non-terminal consumer needs to render its own dialog.
    #[tokio::test]
    async fn delete_gate_announces_and_writes_the_question() {
        let buf = Buf::default();
        let log = crate::log::Log::for_sink(
            crate::cli::resolve::ColorMode::Plain,
            Box::new(buf.clone()),
        );
        let route = std::sync::Arc::new(Recorder {
            answer: "y".into(),
            seen: std::sync::Mutex::new(Vec::new()),
        });

        let mut t = Tombstones::default();
        t.hooks.insert("legacy-export".to_string(), 9137);
        let out = crate::cli::stdin_coord::with_route(route.clone(), async {
            confirm_or_refuse(&t, true, false, &log)
        })
        .await
        .unwrap();
        assert!(matches!(out, ConfirmOutcome::Proceed));

        let text = buf.text();
        assert!(
            text.contains("Proceed with deletion? [y/N] "),
            "question missing from the sink: {text:?}"
        );

        let seen = route.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "expected exactly one announce: {seen:?}");
        assert_eq!(seen[0].kind, crate::cli::stdin_coord::PromptKind::DeleteGate);
        assert_eq!(seen[0].question, "Proceed with deletion? [y/N] ");
        assert_eq!(
            seen[0].keys,
            vec![
                crate::cli::stdin_coord::PromptKey::new('y', "delete them"),
                crate::cli::stdin_coord::PromptKey::new('n', "cancel"),
            ]
        );
    }

    /// Byte-exact pin of the object-delete gate: the event line, one row per
    /// tombstone in delete order, and the question. See
    /// `crate::cli::prompt_pin`.
    #[tokio::test]
    async fn delete_gate_prompt_bytes_are_pinned() {
        let buf = Buf::default();
        let log = crate::cli::prompt_pin::pinned_log(Box::new(buf.clone()));
        let route = std::sync::Arc::new(crate::cli::prompt_pin::CannedRoute("y"));

        let mut t = Tombstones::default();
        t.hooks.insert("legacy-export".to_string(), 9137);
        t.queues.insert("invoices".to_string(), 4210);
        let out = crate::cli::stdin_coord::with_route(route, async {
            confirm_or_refuse(&t, true, false, &log)
        })
        .await
        .unwrap();
        assert!(matches!(out, ConfirmOutcome::Proceed));

        crate::cli::prompt_pin::pin("delete_gate", &buf.text());
    }

    /// Byte-exact pin of the delete-drift resolver.
    #[tokio::test]
    async fn delete_drift_prompt_bytes_are_pinned() {
        let buf = Buf::default();
        let log = crate::cli::prompt_pin::pinned_log(Box::new(buf.clone()));
        let route = std::sync::Arc::new(crate::cli::prompt_pin::CannedRoute("k"));

        let choice = crate::cli::stdin_coord::with_route(route, async {
            resolve_delete_drift(&log, true, "hooks", "legacy-export")
        })
        .await
        .unwrap();
        assert!(matches!(choice, DeleteDriftChoice::KeepDelete));

        crate::cli::prompt_pin::pin("delete_drift", &buf.text());
    }

    /// Same genuine-coverage shape for the delete-drift resolver.
    #[tokio::test]
    async fn delete_drift_announces_and_writes_the_question() {
        let buf = Buf::default();
        let log = crate::log::Log::for_sink(
            crate::cli::resolve::ColorMode::Plain,
            Box::new(buf.clone()),
        );
        let route = std::sync::Arc::new(Recorder {
            answer: "k".into(),
            seen: std::sync::Mutex::new(Vec::new()),
        });

        let choice = crate::cli::stdin_coord::with_route(route.clone(), async {
            resolve_delete_drift(&log, true, "hooks", "legacy-export")
        })
        .await
        .unwrap();
        assert!(matches!(choice, DeleteDriftChoice::KeepDelete));

        let text = buf.text();
        assert!(
            text.contains("[k]eep delete  [r]estore  [s]kip  [a]bort > "),
            "question missing from the sink: {text:?}"
        );

        let seen = route.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "expected exactly one announce: {seen:?}");
        assert_eq!(seen[0].kind, crate::cli::stdin_coord::PromptKind::DeleteDrift);
        assert_eq!(
            seen[0].keys,
            vec![
                crate::cli::stdin_coord::PromptKey::new('k', "keep delete"),
                crate::cli::stdin_coord::PromptKey::new('r', "restore"),
                crate::cli::stdin_coord::PromptKey::new('s', "skip"),
                crate::cli::stdin_coord::PromptKey::new('a', "abort"),
            ]
        );
    }
}
