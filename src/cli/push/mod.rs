use crate::api::RossumClient;
use crate::log::Log;
use crate::paths::Paths;
use crate::state::Lockfile;
use anyhow::{Context, Result};
use std::sync::Arc;

pub mod deletes;
pub mod relink;
mod concurrent;
mod email_templates;
mod engine_fields;
mod engines;
mod hooks;
/// Re-exported for the `--dry-run` planner in [`crate::cli::sync`], which needs
/// to predict the hooks secrets-only pass network-free.
pub(crate) use hooks::plan_secret_pushes;
mod inboxes;
mod labels;
pub mod mdh;
pub mod mdh_data;
mod organization;
mod queues;
mod rules;
mod saved_views;
pub mod scan;
mod schemas;
mod workspaces;

/// Top-level fields a PATCH asked to change that the server kept at their old
/// value: `sent` differs from `before`, and `after` (the re-fetched body)
/// still equals `before`. A 200 does not prove a field was applied, and the
/// write-back would otherwise overwrite the local edit with the old value
/// without a word.
pub(crate) fn ignored_fields(
    sent: &serde_json::Value,
    before: &serde_json::Value,
    after: &serde_json::Value,
) -> Vec<String> {
    let Some(sent) = sent.as_object() else {
        return Vec::new();
    };
    sent.iter()
        // `patch_json` drops the self-identity before sending, so a local
        // `id`/`url` never reached the server.
        .filter(|(k, _)| !matches!(k.as_str(), "id" | "url"))
        .filter(|(k, v)| {
            let old = before.get(k.as_str());
            !old.is_some_and(|o| loosely_equal(o, v)) && after.get(k.as_str()) == old
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// Equality up to what the server normalizes on write: trailing whitespace in
/// strings, and `140` vs `140.0`. A field that differs from the remote only
/// that way is not a change the user asked for, so it must not be reported as
/// ignored.
fn loosely_equal(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.trim_end() == y.trim_end(),
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| loosely_equal(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| loosely_equal(v, w)))
        }
        _ => a == b,
    }
}

/// Warn when a PATCH answered 2xx but `after` still shows the old value for a
/// field `sent` changed (see [`ignored_fields`]). `what` names the object in
/// the log line, e.g. `inbox/<slug>`. Diagnostics only: a body that fails to
/// serialize is skipped, never an error.
pub(crate) fn warn_ignored(
    progress: &crate::log::Log,
    what: &str,
    sent: &impl serde::Serialize,
    before: &impl serde::Serialize,
    after: &impl serde::Serialize,
) {
    let (Ok(sent), Ok(before), Ok(after)) = (
        serde_json::to_value(sent),
        serde_json::to_value(before),
        serde_json::to_value(after),
    ) else {
        return;
    };
    let ignored = ignored_fields(&sent, &before, &after);
    if !ignored.is_empty() {
        progress.event(
            crate::log::Action::Warn,
            &format!(
                "{what}: the server accepted the PATCH but kept its old value for {}",
                ignored.join(", ")
            ),
        );
    }
}

/// Push phase: run each kind's push driver in dependency order. Called
/// by `cli::sync::execute` after the classifier identifies local edits
/// and creates; the executor builds a `ChangeList` from classified items
/// and delegates here.
///
/// Each per-kind driver owns its own `Phase` from the shared
/// `ProgressLog`; dispatch order matches the dependency graph
/// (workspaces → engines → engine fields → schemas → queues → queue-children
/// → org-level leaves).
///
/// `catalog_hooks` is the Phase-1 hook list, threaded into the hooks
/// driver so its store-extension orphan check can avoid a redundant
/// `list_hooks` call. Per-PATCH drift checks still re-list independently.
pub(crate) async fn push_classified(
    paths: &Paths,
    client: &RossumClient,
    lockfile: &mut Lockfile,
    env: &str,
    interactive: bool,
    changes: &scan::ChangeList,
    catalog_hooks: &[crate::model::Hook],
    relink: &mut Vec<relink::DeferredRelink>,
    progress: &Arc<Log>,
) -> Result<(usize, usize)> {
    // Aggregate (pushed, skipped) across every per-kind driver so the
    // cycle summary can reconcile its plan-time tally with what was
    // actually written (a driver skip — refused create, drift skip,
    // adopt-remote — is NOT a remote change).
    let mut pushed = 0usize;
    let mut skipped = 0usize;
    let mut tally = |counts: (usize, usize)| {
        pushed += counts.0;
        skipped += counts.1;
    };
    // Destructured exhaustively — NO `..` rest pattern, on purpose. This
    // function is the CONSUMER end of `scan::change_list_from_classified`: a
    // kind added to `ChangeList` and to the producer but forgotten here has a
    // dead push half, and every enforcement test in this design still passes,
    // because they all check the producer. Naming every field turns that
    // omission into a compile error instead. If a field ever genuinely has no
    // consumer here, prefix its binding with `_` and say why — do not reach for
    // `..`.
    let scan::ChangeList {
        workspaces,
        queues,
        schemas,
        inboxes,
        email_templates,
        hooks,
        rules,
        labels,
        saved_views,
        engines,
        engine_fields,
        organization,
    } = changes;
    if !workspaces.is_empty() {
        tally(workspaces::push(paths, client, lockfile, interactive, workspaces, progress, env).await
            .with_context(|| format!("pushing workspaces for env '{env}'"))?);
    }
    // Engines and their fields before schemas and queues. Unlike every other
    // edge in this graph, this one is not a reference the payload carries — it
    // is a server-side CONTENT check: `POST /queues` validates the queue's
    // schema against the bound engine's field NAMES and refuses the create with
    //
    //   non_field_errors: Engine (id: N) restriction: extracted field
    //   '<schema field>' is not present among names of engine fields
    //
    // for every extracted field the engine does not (yet) have. Pushed last, as
    // "org-level leaves", the engine fields did not exist yet and a promote into
    // a fresh env died on its first queue.
    //
    // The failure was hidden while the target engine was NEW: the queue's
    // `engine` ref could not resolve, so it was deferred out of the create body
    // and PATCHed by `run_relink` after the engine fields existed. It bites only
    // when the engine ALREADY exists in the target — the ref resolves, the
    // binding ships with the create, and it is validated against an engine whose
    // fields are still queued behind the queue.
    //
    // Safe this early: an engine's only queue-facing ref is `training_queues`,
    // which lives in `extra` and is therefore deferrable by construction (see
    // `relink::undeferrable`, where `engines` protects only `url`), so it is
    // postponed to the relink pass exactly as a hook's `run_after` is. Engine
    // fields reference only their engine, which is why they stay behind it.
    if !engines.is_empty() {
        tally(engines::push(paths, client, lockfile, interactive, engines, relink, progress, env).await
            .with_context(|| format!("pushing engines for env '{env}'"))?);
    }
    if !engine_fields.is_empty() {
        tally(engine_fields::push(paths, client, lockfile, interactive, engine_fields, progress, env).await
            .with_context(|| format!("pushing engine fields for env '{env}'"))?);
    }
    if !schemas.is_empty() {
        tally(schemas::push(paths, client, lockfile, interactive, schemas, progress, env).await
            .with_context(|| format!("pushing schemas for env '{env}'"))?);
    }
    if !queues.is_empty() {
        tally(queues::push(paths, client, lockfile, interactive, queues, relink, progress, env).await
            .with_context(|| format!("pushing queues for env '{env}'"))?);
    }
    if !inboxes.is_empty() {
        tally(inboxes::push(paths, client, lockfile, interactive, inboxes, progress, env).await
            .with_context(|| format!("pushing inboxes for env '{env}'"))?);
    }
    if !email_templates.is_empty() {
        tally(email_templates::push(paths, client, lockfile, interactive, email_templates, progress, env).await
            .with_context(|| format!("pushing email templates for env '{env}'"))?);
    }
    // Hooks always go through `push` (no early-skip on empty `changes`)
    // so the secrets-only pass inside it can detect changes to
    // `secrets/<env>.hook-secrets.json` that aren't accompanied by a
    // hook JSON/code edit. The function returns (0, 0) when neither
    // content nor secrets have drifted.
    tally(hooks::push(paths, client, lockfile, interactive, hooks, catalog_hooks, relink, progress, env)
        .await
        .with_context(|| format!("pushing hooks for env '{env}'"))?);
    // Labels before rules: a rule's `actions` can reference a label
    // (`rdc://labels/<slug>`), and `rules::push` resolves those refs at
    // create time. Labels are leaf objects (they reference only the
    // organization), so creating them first lets the rule's label refs
    // resolve against the lockfile instead of failing with "Invalid
    // hyperlink - No URL match".
    if !labels.is_empty() {
        tally(labels::push(paths, client, lockfile, interactive, labels, progress, env).await
            .with_context(|| format!("pushing labels for env '{env}'"))?);
    }
    // A saved view's `queues_filter` can reference a queue, already pushed
    // above; unlike labels/rules it does not itself participate in another
    // kind's create-time ref resolution, so its ordering here is otherwise free.
    if !saved_views.is_empty() {
        tally(saved_views::push(paths, client, lockfile, interactive, saved_views, progress, env).await
            .with_context(|| format!("pushing saved views for env '{env}'"))?);
    }
    if !rules.is_empty() {
        tally(rules::push(paths, client, lockfile, interactive, rules, progress, env).await
            .with_context(|| format!("pushing rules for env '{env}'"))?);
    }
    // Last: the organization singleton references nothing and nothing
    // references it, so its ordering relative to every other kind is free.
    if let Some(path) = organization {
        tally(
            organization::push(paths, client, lockfile, path, progress, env)
                .await
                .with_context(|| format!("pushing organization for env '{env}'"))?,
        );
    }
    Ok((pushed, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ignored_fields_skips_self_identity() {
        let before = json!({ "id": 5, "url": "https://x/labels/5", "name": "A" });
        let sent = json!({ "id": 0, "url": "rdc://labels/a", "name": "A" });
        assert!(ignored_fields(&sent, &before, &before).is_empty());
    }

    /// A hook whose code differs from the remote only by a trailing newline
    /// (clean under rdc's EOF-insensitive hashing) is not an edit, so the
    /// server trimming it back must not be reported.
    #[test]
    fn ignored_fields_tolerates_server_normalization() {
        let before = json!({ "name": "A", "config": { "code": "x = 1", "width": 140.0 } });
        let sent = json!({ "name": "B", "config": { "code": "x = 1\n", "width": 140 } });
        let after = json!({ "name": "B", "config": { "code": "x = 1", "width": 140.0 } });
        assert!(ignored_fields(&sent, &before, &after).is_empty());
    }

    #[test]
    fn ignored_fields_reports_a_nested_edit_the_server_dropped() {
        let before = json!({ "config": { "code": "x = 1" } });
        let sent = json!({ "config": { "code": "x = 2" } });
        assert_eq!(ignored_fields(&sent, &before, &before), vec!["config".to_string()]);
    }

    /// A key the remote never had, sent and not echoed back, is reported: the
    /// server does not know the field.
    #[test]
    fn ignored_fields_reports_an_unknown_key() {
        let before = json!({ "name": "A" });
        let sent = json!({ "name": "A", "colour": "red" });
        assert_eq!(ignored_fields(&sent, &before, &before), vec!["colour".to_string()]);
    }
}
