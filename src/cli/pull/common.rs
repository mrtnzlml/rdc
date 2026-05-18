use crate::api::{ApiError, RossumClient};
use crate::paths::Paths;
use crate::progress::ProgressLog;
use crate::state::{content_hash, Lockfile, ObjectEntry};
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// If `result` is a 403 permission_denied from the Rossum API, log a warning
/// and return an empty list — the kind is unavailable to this token, but
/// other kinds should still pull. Otherwise propagate the error unchanged.
pub fn skip_on_permission_denied<T>(
    result: Result<Vec<T>>,
    kind: &str,
    progress: &Arc<ProgressLog>,
) -> Result<Vec<T>> {
    match result {
        Ok(v) => Ok(v),
        Err(e) => {
            let is_403 = e.chain().any(|c| {
                c.downcast_ref::<ApiError>()
                    .map(|api| matches!(api, ApiError::Status { status: 403, .. }))
                    .unwrap_or(false)
            });
            if is_403 {
                progress.warn(format!("! skipping {kind}: token lacks permission (403)"));
                Ok(Vec::new())
            } else {
                Err(e)
            }
        }
    }
}

/// Shared state passed through every per-kind pull driver.
pub struct PullCtx<'a> {
    pub paths: &'a Paths,
    pub client: &'a RossumClient,
    pub lockfile: &'a mut Lockfile,
    /// Map of queue URL → `(ws_slug, q_slug)`, populated by the queues driver
    /// and consumed by drivers for queue-nested kinds (currently
    /// email_templates). Empty until queues run.
    pub queue_locations: BTreeMap<String, (String, String)>,
    /// Per-env overlay loaded once at pull entry. When `Some`, pull drivers
    /// strip overlay-managed paths from the incoming remote bytes before
    /// hashing/writing — this keeps `<env>` snapshots in their canonical
    /// pre-overlay form so cross-env diffs and deploys are quiet (spec
    /// §9.3). `None` when the env has no `overlay.toml`.
    pub overlay: Option<crate::overlay::Overlay>,
    /// When true, conflicts trigger an interactive [k]/[r]/[e]/[s]/[a]
    /// prompt (spec §8.3). False on non-TTY or when `--yes` was passed —
    /// in that case `apply_pull_action` falls back to the legacy
    /// shadow-file behavior. Drivers consult `ctx.interactive` and pass
    /// it to `apply_pull_action`.
    pub interactive: bool,
}

/// Bound for the per-item async fan-out in drivers that pipeline sub-fetches
/// (queues fetching schema + inbox, mdh fetching indexes + search-indexes).
/// Empirically saturates upstream — going higher doesn't help because
/// `list_*` calls remain serial.
pub const PULL_FANOUT: usize = 5;

/// All kinds listed from one env's API. Produced by Phase 1 of pull,
/// consumed by Phase 2 (pull) or by the sync classifier.
///
/// `schemas_by_queue_id` and `inboxes_by_queue_id` are pre-fetched by
/// `list_remote` because the Rossum API has no `/schemas/` or `/inboxes/`
/// listing — each must be fetched per-queue. Pre-fetching here lets the
/// sync classifier compute remote hashes without having to issue an
/// additional fetch per queue; the existing pull driver still re-fetches
/// (we'd duplicate work but the alternative is threading the prefetch
/// through the driver, which expands the per-kind API surface).
pub struct RemoteCatalog {
    pub organization: crate::model::Organization,
    pub workspaces: Vec<crate::model::Workspace>,
    pub queues: Vec<crate::model::Queue>,
    /// Map of queue id → schema body. `None` when the queue has no
    /// schema (orphan queues, hidden queues), or when the schema fetch
    /// failed (rare; surfaces as an orphan in `pull::queues::process`).
    pub schemas_by_queue_id: std::collections::BTreeMap<u64, crate::model::Schema>,
    /// Map of queue id → inbox body. Most queues have no inbox.
    pub inboxes_by_queue_id: std::collections::BTreeMap<u64, crate::model::Inbox>,
    pub hooks: Vec<crate::model::Hook>,
    pub rules: Vec<crate::model::Rule>,
    pub labels: Vec<crate::model::Label>,
    pub engines: Vec<crate::model::Engine>,
    pub engine_fields: Vec<crate::model::EngineField>,
    pub workflows: Vec<crate::model::Workflow>,
    pub workflow_steps: Vec<crate::model::WorkflowStep>,
    pub email_templates: Vec<crate::model::EmailTemplate>,
    pub mdh: crate::cli::pull::mdh::MdhListed,
}

/// Phase 1 of pull: list every kind from the env's API. The catalog is
/// consumed by the sync classifier and executor.
///
/// Every list call gets a spinner inside a single "listing remote" phase so
/// the user sees animation while rdc is waiting on the API. The phase header
/// prints once at the top; each per-kind line resolves to `[ok] <kind> <N>`
/// after its API call returns.
///
/// Listing order is fixed so cross-env diffs stay deterministic across
/// runs.
pub async fn list_remote(
    ctx: &mut PullCtx<'_>,
    env_cfg: &crate::config::EnvConfig,
    env: &str,
    token: &str,
    progress: &Arc<crate::progress::ProgressLog>,
) -> Result<RemoteCatalog> {
    let phase = progress.phase("listing remote");

    let sp = phase.item("organization");
    let organization = crate::cli::pull::organization::list(ctx, env_cfg.org_id, progress).await
        .with_context(|| format!("listing organization for env '{env}'"))?;
    sp.finish_ok(organization.name.clone());

    let sp = phase.item("workspaces");
    let workspaces = crate::cli::pull::workspaces::list(ctx, progress).await
        .with_context(|| format!("listing workspaces for env '{env}'"))?;
    sp.finish_ok(workspaces.len().to_string());

    let sp = phase.item("queues");
    let queues = crate::cli::pull::queues::list(ctx, progress).await
        .with_context(|| format!("listing queues for env '{env}'"))?;
    sp.finish_ok(queues.len().to_string());

    let sp = phase.item("hooks");
    let hooks = crate::cli::pull::hooks::list(ctx, progress).await
        .with_context(|| format!("listing hooks for env '{env}'"))?;
    sp.finish_ok(hooks.len().to_string());

    let sp = phase.item("rules");
    let rules = crate::cli::pull::rules::list(ctx, progress).await
        .with_context(|| format!("listing rules for env '{env}'"))?;
    sp.finish_ok(rules.len().to_string());

    let sp = phase.item("labels");
    let labels = crate::cli::pull::labels::list(ctx, progress).await
        .with_context(|| format!("listing labels for env '{env}'"))?;
    sp.finish_ok(labels.len().to_string());

    let sp = phase.item("engines");
    let engines = crate::cli::pull::engines::list(ctx, progress).await
        .with_context(|| format!("listing engines for env '{env}'"))?;
    sp.finish_ok(engines.len().to_string());

    let sp = phase.item("engine fields");
    let engine_fields = crate::cli::pull::engine_fields::list(ctx, progress).await
        .with_context(|| format!("listing engine fields for env '{env}'"))?;
    sp.finish_ok(engine_fields.len().to_string());

    let sp = phase.item("workflows");
    let workflows = crate::cli::pull::workflows::list(ctx, progress).await
        .with_context(|| format!("listing workflows for env '{env}'"))?;
    sp.finish_ok(workflows.len().to_string());

    let sp = phase.item("workflow steps");
    let workflow_steps = crate::cli::pull::workflow_steps::list(ctx, progress).await
        .with_context(|| format!("listing workflow steps for env '{env}'"))?;
    sp.finish_ok(workflow_steps.len().to_string());

    let sp = phase.item("email templates");
    let email_templates = crate::cli::pull::email_templates::list(ctx, progress).await
        .with_context(|| format!("listing email templates for env '{env}'"))?;
    sp.finish_ok(email_templates.len().to_string());

    let sp = phase.item("mdh datasets");
    let mdh = crate::cli::pull::mdh::list(env_cfg, token, progress).await
        .with_context(|| format!("listing MDH datasets for env '{env}'"))?;
    sp.finish_ok(mdh.collections.len().to_string());

    // Per-queue schema + inbox prefetch. The Rossum API has no `/schemas/`
    // or `/inboxes/` listing — each must be fetched by id. Doing it here
    // gives the sync classifier accurate remote hashes for queue-nested
    // kinds; the pull driver still re-fetches inside its own process loop
    // (extra work but the driver's signature stays unchanged).
    //
    // Concurrency mirrors `pull::queues::process`'s sub-phase B:
    // `buffer_unordered(PULL_FANOUT)` over (queue_id, schema_id?, inbox_id?).
    // A single spinner shows progress while the parallel batch runs; without
    // it the user sees nothing for several seconds during a tree of dozens.
    let (schemas_by_queue_id, inboxes_by_queue_id) =
        prefetch_queue_children(ctx.client, &queues, &phase, progress).await
            .with_context(|| format!("prefetching schemas + inboxes for env '{env}'"))?;

    Ok(RemoteCatalog {
        organization, workspaces, queues, schemas_by_queue_id, inboxes_by_queue_id,
        hooks, rules, labels,
        engines, engine_fields, workflows, workflow_steps,
        email_templates, mdh,
    })
}

/// Concurrent per-queue schema + inbox fetch. Mirrors the bounded fan-out
/// in `pull::queues::process` so the catalog's prefetch and the driver
/// keep parity on rate and ordering. Queues with no schema / no inbox
/// contribute nothing to the returned maps.
///
/// A single spinner ("schemas + inboxes") is held by the calling task and
/// updates its message via `set_message` as each parallel fetch completes,
/// so the user sees a `(N/M)` running counter while the batch is in flight.
/// `progress` is threaded into each `get_schema` / `get_inbox` call so any
/// 429 retry warning renders cleanly above the spinner instead of tearing
/// the surrounding draw region.
async fn prefetch_queue_children(
    client: &RossumClient,
    queues: &[crate::model::Queue],
    phase: &crate::progress::Phase,
    progress: &Arc<crate::progress::ProgressLog>,
) -> Result<(
    std::collections::BTreeMap<u64, crate::model::Schema>,
    std::collections::BTreeMap<u64, crate::model::Inbox>,
)> {
    use futures::stream::{StreamExt, TryStreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Build (queue_id, schema_id?, inbox_id?) triples once so the future
    // body doesn't borrow `queues`.
    let triples: Vec<(u64, String, Option<u64>, Option<u64>)> = queues
        .iter()
        .map(|q| {
            let s = q
                .schema
                .as_deref()
                .map(parse_id_from_url)
                .transpose()
                .ok()
                .flatten();
            let i = q
                .inbox
                .as_deref()
                .map(parse_id_from_url)
                .transpose()
                .ok()
                .flatten();
            (q.id, q.name.clone(), s, i)
        })
        .collect();
    let total = triples.len();

    // Nothing to fetch — skip the spinner entirely so the listing phase
    // doesn't show a redundant `(0/0)` line.
    if total == 0 {
        return Ok((std::collections::BTreeMap::new(), std::collections::BTreeMap::new()));
    }

    // Spinner shared across the parallel batch. Workers update only its
    // message; the calling task holds ownership and calls finish_ok at the
    // end. `Spinner::set_message` takes `&self`, so an `Arc<Spinner>` is
    // safe to clone into each worker future.
    //
    // The spinner is constructed with the bare base name "schemas + inboxes".
    // Workers update the displayed message to `schemas + inboxes (N/total)`
    // as fetches resolve; on finalize the bar's transient message is dropped
    // and the `[ok]` line uses the base name + summary (see
    // `Spinner::finish_ok`).
    let sp = Arc::new(phase.item("schemas + inboxes"));
    let done = Arc::new(AtomicUsize::new(0));

    let fetched_result: Result<Vec<(u64, Option<crate::model::Schema>, Option<crate::model::Inbox>)>> =
        futures::stream::iter(triples)
            .map(|(qid, qname, sid, iid)| {
                let sp = sp.clone();
                let done = done.clone();
                let progress = progress.clone();
                async move {
                    let schema = match sid {
                        Some(id) => Some(
                            client
                                .get_schema(id, Some(progress.clone()))
                                .await
                                .with_context(|| {
                                    format!("prefetching schema {id} for queue '{qname}'")
                                })?,
                        ),
                        None => None,
                    };
                    let inbox = match iid {
                        Some(id) => Some(
                            client
                                .get_inbox(id, Some(progress.clone()))
                                .await
                                .with_context(|| {
                                    format!("prefetching inbox {id} for queue '{qname}'")
                                })?,
                        ),
                        None => None,
                    };
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    sp.set_message(format!("schemas + inboxes ({n}/{total})"));
                    Ok::<_, anyhow::Error>((qid, schema, inbox))
                }
            })
            .buffer_unordered(PULL_FANOUT)
            .try_collect()
            .await;

    let fetched = match fetched_result {
        Ok(v) => v,
        Err(e) => {
            // Spinner is in an Arc; we can't move it out to call finish_*
            // (which consumes by value). Drop our reference; if workers
            // already dropped theirs, Drop emits "(cancelled)". Otherwise
            // the spinner finalizes when the last Arc is dropped.
            drop(sp);
            return Err(e);
        }
    };

    // Finalize the spinner. `Arc::try_unwrap` can fail only if a worker
    // future still holds a reference, which can't happen here (we awaited
    // the whole stream). Fall through to drop on the unlikely path.
    if let Ok(spinner) = Arc::try_unwrap(sp) {
        spinner.finish_ok(format!("{total} fetched"));
    }

    let mut schemas = std::collections::BTreeMap::new();
    let mut inboxes = std::collections::BTreeMap::new();
    for (qid, s, i) in fetched {
        if let Some(s) = s {
            schemas.insert(qid, s);
        }
        if let Some(i) = i {
            inboxes.insert(qid, i);
        }
    }
    Ok((schemas, inboxes))
}

/// If `paths` is `Some` and non-empty, strip those overlay-managed dotted
/// paths from `bytes` (parse to Value, strip, re-serialize). Otherwise
/// return `bytes` unchanged. Used by every writable-kind pull driver to
/// keep the snapshot in its canonical pre-overlay form (spec §9.3).
pub fn maybe_strip_overlay(
    bytes: Vec<u8>,
    paths: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
) -> Result<Vec<u8>> {
    let Some(paths) = paths else { return Ok(bytes); };
    if paths.is_empty() {
        return Ok(bytes);
    }
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)
        .context("parsing JSON for overlay strip")?;
    crate::overlay::strip_paths(&mut value, paths);
    let mut out = serde_json::to_vec_pretty(&value)
        .context("re-serializing post overlay strip")?;
    out.push(b'\n');
    Ok(out)
}

/// Record an object in the lockfile under the given kind/slug.
pub fn record_object(
    lockfile: &mut Lockfile,
    kind: &str,
    slug: &str,
    id: u64,
    url: Option<String>,
    modified_at: Option<String>,
    content_hash: Option<String>,
) {
    lockfile.upsert(
        kind,
        slug,
        ObjectEntry { id, url, modified_at, content_hash, secrets_hash: None },
    );
}

/// Parse the trailing numeric ID out of a Rossum API URL, e.g.
/// `https://x.rossum.app/api/v1/schemas/1234` -> `1234`.
pub fn parse_id_from_url(url: &str) -> Result<u64> {
    let trimmed = url.trim_end_matches('/');
    let last = trimmed
        .rsplit('/')
        .next()
        .ok_or_else(|| anyhow!("URL has no path segments: {url}"))?;
    last.parse::<u64>()
        .map_err(|e| anyhow!("URL trailing segment '{last}' is not a u64: {e}"))
}

/// Outcome of a three-way comparison for a single object on pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullAction {
    /// First pull, or local hasn't been edited, or remote is unchanged from base —
    /// safe to write the remote bytes.
    Write,
    /// Local has edits and remote is unchanged from base — keep the local file.
    KeepLocal,
    /// Both local and remote have diverged from base — real conflict.
    Conflict,
    /// Local and remote canonicalize to the same bytes (only noise fields
    /// like `modified_at` differ). Skip the write to preserve on-disk
    /// byte-stability across re-pulls.
    NoChange,
}

/// Decide what to do on pull for a single object.
///
/// `local_path` — the on-disk JSON file path (may not exist).
/// `base_hash` — the lockfile's recorded content_hash for this object (None if no prior entry).
/// `remote_bytes` — the just-serialized remote candidate bytes that would be written.
///
/// Returns: `(action, remote_hash)`. The remote_hash is always returned because the
/// caller may need it for the lockfile.
pub fn decide_pull_action(
    local_path: &Path,
    base_hash: Option<&str>,
    remote_bytes: &[u8],
) -> Result<(PullAction, String)> {
    let remote_hash = content_hash(remote_bytes);

    let Some(base) = base_hash else {
        return Ok((PullAction::Write, remote_hash));
    };

    if !local_path.exists() {
        return Ok((PullAction::Write, remote_hash));
    }

    let local_bytes = std::fs::read(local_path)
        .with_context(|| format!("reading {}", local_path.display()))?;
    let local_hash = content_hash(&local_bytes);

    // Short-circuit: canonicalized local == canonicalized remote means
    // any difference is noise (modified_at etc.). Don't rewrite the file.
    if local_hash == remote_hash {
        return Ok((PullAction::NoChange, remote_hash));
    }

    // local_hash == remote_hash is already handled above as NoChange, so
    // we only need to branch on which side diverged from the base.
    let local_matches_base = local_hash == base;
    let remote_matches_base = remote_hash == base;

    let action = match (local_matches_base, remote_matches_base) {
        (true, _) => PullAction::Write,
        (false, true) => PullAction::KeepLocal,
        (false, false) => PullAction::Conflict,
    };

    Ok((action, remote_hash))
}

/// Apply the decision to the filesystem and return the hash that should be
/// recorded in the lockfile (which differs depending on the action).
///
/// On [`PullAction::Conflict`] with `interactive == true`, the function
/// invokes the spec §8.3 resolver TUI. With `interactive == false` (CI,
/// non-TTY, or `--yes`), it preserves the legacy shadow-file behavior:
/// writes `<file>.<env>` next to the local file, keeps local, and returns
/// the *prior base hash* (`base_hash`) so the caller's `record_object` is
/// a no-op for the lockfile entry — the conflict re-surfaces on the next
/// pull. If `base_hash` is `None` (defensive — a conflict presupposes a
/// prior base), the function falls back to today's behavior and returns
/// the local hash so the lockfile still gets a sensible entry.
///
/// The same preserve-base rule applies to a
/// [`crate::cli::resolve::Resolution::EditWithMarkers`] outcome: the
/// merged bytes contain unresolved markers, so the lockfile must not
/// advance.
pub fn apply_pull_action(
    action: PullAction,
    local_path: &Path,
    remote_bytes: &[u8],
    remote_hash: String,
    interactive: bool,
    progress: &Arc<ProgressLog>,
    env: &str,
    base_hash: Option<&str>,
) -> Result<String> {
    use crate::snapshot::writer::write_atomic;
    match action {
        PullAction::Write => {
            write_atomic(local_path, remote_bytes)?;
            Ok(remote_hash)
        }
        PullAction::KeepLocal => {
            let local_bytes = std::fs::read(local_path)
                .with_context(|| format!("reading {}", local_path.display()))?;
            Ok(content_hash(&local_bytes))
        }
        PullAction::NoChange => {
            // Local and remote canonicalize equal — preserve disk bytes.
            // Hash is identical to remote_hash by construction.
            Ok(remote_hash)
        }
        PullAction::Conflict => {
            if interactive {
                resolve_conflict_interactive(
                    local_path,
                    remote_bytes,
                    &remote_hash,
                    progress,
                    env,
                    base_hash,
                )
            } else {
                shadow_file_conflict(local_path, remote_bytes, progress, env, base_hash)
            }
        }
    }
}

/// The legacy shadow-file behavior for a conflict: write
/// `<file>.<env>`, keep local on disk. Used when `interactive == false`
/// (CI/non-TTY/--yes) and as a fallback from the resolver when the user
/// picks `[s]kip`.
///
/// Returns the *prior* base hash (`base_hash`) so the caller's
/// `record_object` is a no-op — the lockfile entry must not advance on a
/// shadow-skip, otherwise the next pull misclassifies the slug as clean
/// and the conflict is silently swallowed. Falls back to the local hash
/// only when `base_hash` is `None` (defensive — conflicts presuppose a
/// prior base).
fn shadow_file_conflict(
    local_path: &Path,
    remote_bytes: &[u8],
    progress: &Arc<ProgressLog>,
    env: &str,
    base_hash: Option<&str>,
) -> Result<String> {
    use crate::snapshot::writer::write_atomic;
    let conflict_path = crate::paths::shadow_path_for(local_path, env);
    write_atomic(&conflict_path, remote_bytes)?;
    progress.warn(format!(
        "! {} conflict: local preserved, remote at {} (lockfile base preserved; re-run to resolve)",
        local_path.display(),
        conflict_path.display(),
    ));
    if let Some(prior) = base_hash {
        return Ok(prior.to_string());
    }
    // Defensive fallback: no prior base recorded (shouldn't happen for a
    // real conflict; conflicts presuppose `(local != base, remote != base)`).
    // Use local hash so the lockfile still gets a sensible entry.
    let local_bytes = std::fs::read(local_path)
        .with_context(|| format!("reading {}", local_path.display()))?;
    Ok(content_hash(&local_bytes))
}

/// Drive the spec §8.3 resolver TUI on stdin/stderr. On
/// [`crate::cli::resolve::Resolution::Abort`] this returns a
/// [`crate::cli::resolve::PullAborted`]-wrapping anyhow error so the
/// pull runner can downcast and skip lockfile.save().
fn resolve_conflict_interactive(
    local_path: &Path,
    remote_bytes: &[u8],
    remote_hash: &str,
    progress: &Arc<ProgressLog>,
    env: &str,
    base_hash: Option<&str>,
) -> Result<String> {
    use crate::cli::resolve::{prompt_resolve, PullAborted, Resolution};
    use crate::snapshot::writer::write_atomic;

    let stdin = std::io::stdin();
    let stderr = std::io::stderr();
    let resolution = prompt_resolve(
        stdin.lock(),
        stderr.lock(),
        1, // No global counter yet — drivers don't share an index/total.
        1,
        local_path,
        remote_bytes,
        env,
    )?;

    match resolution {
        Resolution::KeepLocal => {
            let local_bytes = std::fs::read(local_path)
                .with_context(|| format!("reading {}", local_path.display()))?;
            Ok(content_hash(&local_bytes))
        }
        Resolution::KeepRemote => {
            write_atomic(local_path, remote_bytes)?;
            Ok(remote_hash.to_string())
        }
        Resolution::Edit(edited) => {
            write_atomic(local_path, &edited)?;
            Ok(content_hash(&edited))
        }
        Resolution::EditWithMarkers(edited) => {
            // Hunk-by-hunk walker with at least one skipped hunk — bytes
            // intentionally retain `<<<<<<<` / `=======` / `>>>>>>>`
            // markers. Writing those bytes is fine, but the lockfile
            // base MUST NOT advance: the next pull/sync needs to see the
            // marker-bearing state as still-conflicting so the user keeps
            // getting nudged. Return the prior base (same rule as
            // shadow-skip); fall back to local hash when no prior base.
            write_atomic(local_path, &edited)?;
            if let Some(prior) = base_hash {
                progress.warn(format!(
                    "! {} partially resolved (markers retained); lockfile base preserved; re-run to resolve",
                    local_path.display(),
                ));
                Ok(prior.to_string())
            } else {
                Ok(content_hash(&edited))
            }
        }
        Resolution::Skip => {
            shadow_file_conflict(local_path, remote_bytes, progress, env, base_hash)
        }
        Resolution::Abort => Err(anyhow::Error::new(PullAborted)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_id_basic() {
        assert_eq!(parse_id_from_url("https://x/api/v1/schemas/1234").unwrap(), 1234);
    }

    #[test]
    fn parse_id_with_trailing_slash() {
        assert_eq!(parse_id_from_url("https://x/api/v1/schemas/9/").unwrap(), 9);
    }

    #[test]
    fn parse_id_non_numeric_errors() {
        assert!(parse_id_from_url("https://x/api/v1/schemas/abc").is_err());
    }

    #[test]
    fn first_pull_writes_when_no_base_hash() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        let (action, _hash) = decide_pull_action(&path, None, b"{}").unwrap();
        assert_eq!(action, PullAction::Write);
    }

    #[test]
    fn write_when_no_local_file_exists() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        let (action, _hash) = decide_pull_action(&path, Some("any-hash"), b"{}").unwrap();
        assert_eq!(action, PullAction::Write);
    }

    #[test]
    fn keep_local_when_only_local_edited() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"{ \"local\": true }").unwrap();
        let remote = b"{}";
        let base = content_hash(remote);
        let (action, _hash) = decide_pull_action(&path, Some(&base), remote).unwrap();
        assert_eq!(action, PullAction::KeepLocal);
    }

    #[test]
    fn write_when_only_remote_changed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        let original = b"{ \"original\": true }";
        std::fs::write(&path, original).unwrap();
        let base = content_hash(original);
        let remote = b"{ \"updated\": true }";
        let (action, _hash) = decide_pull_action(&path, Some(&base), remote).unwrap();
        assert_eq!(action, PullAction::Write);
    }

    #[test]
    fn conflict_when_both_changed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"{ \"local\": true }").unwrap();
        let base = "0".repeat(64);
        let remote = b"{ \"remote\": true }";
        let (action, _hash) = decide_pull_action(&path, Some(&base), remote).unwrap();
        assert_eq!(action, PullAction::Conflict);
    }

    #[test]
    fn apply_write_creates_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        let p = crate::progress::ProgressLog::start("test");
        let h = apply_pull_action(PullAction::Write, &path, b"hello", "h".repeat(64), false, &p, "test", None).unwrap();
        p.finish("test");
        assert_eq!(h, "h".repeat(64));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn apply_conflict_non_interactive_writes_remote_sibling() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local").unwrap();
        // interactive=false → legacy shadow-file behavior.
        let p = crate::progress::ProgressLog::start("test");
        let _ = apply_pull_action(PullAction::Conflict, &path, b"remote", "h".repeat(64), false, &p, "test", None).unwrap();
        p.finish("test");
        assert_eq!(std::fs::read(&path).unwrap(), b"local");
        assert_eq!(
            std::fs::read(dir.path().join("x.json.test")).unwrap(),
            b"remote",
            "shadow file should be named after the env"
        );
    }

    #[test]
    fn skip_on_permission_denied_returns_empty_for_403() {
        let err: Result<Vec<u32>> = Err(anyhow!(ApiError::Status {
            status: 403,
            body: "permission_denied".into()
        }));
        let p = crate::progress::ProgressLog::start("test");
        let out = skip_on_permission_denied(err, "engines", &p).unwrap();
        p.finish("test");
        assert!(out.is_empty());
    }

    #[test]
    fn skip_on_permission_denied_propagates_other_errors() {
        let err: Result<Vec<u32>> = Err(anyhow!(ApiError::Status {
            status: 500,
            body: "boom".into()
        }));
        let p = crate::progress::ProgressLog::start("test");
        assert!(skip_on_permission_denied(err, "engines", &p).is_err());
    }

    #[test]
    fn skip_on_permission_denied_passes_through_ok() {
        let v: Result<Vec<u32>> = Ok(vec![1, 2, 3]);
        let p = crate::progress::ProgressLog::start("test");
        let out = skip_on_permission_denied(v, "engines", &p).unwrap();
        p.finish("test");
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn decide_returns_nochange_when_canonical_local_equals_canonical_remote() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        // Local has modified_at = t1
        std::fs::write(&path, b"{\"name\":\"x\",\"modified_at\":\"t1\"}").unwrap();
        // Remote has modified_at = t2 (newer); same other content
        let remote = b"{\"name\":\"x\",\"modified_at\":\"t2\"}";
        // Base hash matches both (canonical strips modified_at)
        let base = content_hash(remote);
        let (action, _hash) = decide_pull_action(&path, Some(&base), remote).unwrap();
        assert_eq!(action, PullAction::NoChange);
    }

    #[test]
    fn apply_nochange_does_not_modify_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"original").unwrap();
        let original_bytes = std::fs::read(&path).unwrap();
        let p = crate::progress::ProgressLog::start("test");
        let h = apply_pull_action(
            PullAction::NoChange,
            &path,
            b"different remote bytes",
            "h".repeat(64),
            false,
            &p,
            "test",
            None,
        )
        .unwrap();
        p.finish("test");
        assert_eq!(h, "h".repeat(64));
        // Local file unchanged byte-for-byte.
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
    }

    /// A non-interactive (CI / non-TTY / `--yes`) conflict pull writes a
    /// shadow file, keeps local on disk, and — critically — returns the
    /// *prior base hash* so the caller's `record_object` is a no-op. This
    /// ensures the next pull/sync re-classifies the slug as a conflict
    /// instead of silently swallowing it.
    #[test]
    fn shadow_file_skip_does_not_advance_lockfile_base() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        let local_before = b"{\"local\":true}";
        std::fs::write(&path, local_before).unwrap();
        let remote = b"{\"remote\":true}";
        let prior_base = "BASE_HASH_64_chars_".to_string() + &"0".repeat(45);
        assert_eq!(prior_base.len(), 64);
        let p = crate::progress::ProgressLog::start("test");
        let recorded = apply_pull_action(
            PullAction::Conflict,
            &path,
            remote,
            content_hash(remote),
            false, // non-interactive → shadow_file_conflict
            &p,
            "test",
            Some(&prior_base),
        )
        .unwrap();
        p.finish("test");
        // Lockfile must NOT advance — recorded hash equals prior base.
        assert_eq!(recorded, prior_base, "shadow-skip must preserve lockfile base");
        // Shadow file carries remote bytes.
        let shadow = dir.path().join("x.json.test");
        assert!(shadow.exists(), "shadow file should be written");
        assert_eq!(std::fs::read(&shadow).unwrap(), remote);
        // Local file unchanged.
        assert_eq!(std::fs::read(&path).unwrap(), local_before);
    }

    /// Defensive: a conflict can only happen after a prior base exists, but
    /// if `base_hash` is `None` (paranoia, never seen in practice) we fall
    /// back to the local hash so the lockfile still gets a sensible entry.
    /// Documents the fallback contract.
    #[test]
    fn shadow_file_skip_with_no_prior_base_falls_back_to_local_hash() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        let local = b"{\"local\":true}";
        std::fs::write(&path, local).unwrap();
        let remote = b"{\"remote\":true}";
        let p = crate::progress::ProgressLog::start("test");
        let recorded = apply_pull_action(
            PullAction::Conflict,
            &path,
            remote,
            content_hash(remote),
            false,
            &p,
            "test",
            None, // no prior base
        )
        .unwrap();
        p.finish("test");
        assert_eq!(recorded, content_hash(local));
    }

    /// Re-running the same conflict pull twice with shadow-skip must keep
    /// surfacing the conflict — the lockfile entry never advances, so the
    /// classifier sees `(local != base, remote != base)` both times.
    #[test]
    fn two_consecutive_pulls_with_shadow_skip_keep_re_prompting() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        let local = b"{\"local\":true}";
        std::fs::write(&path, local).unwrap();
        let remote = b"{\"remote\":true}";
        let base_hash = "B".repeat(64);
        let p = crate::progress::ProgressLog::start("test");

        // First pull → conflict → shadow-skip.
        let (action1, remote_hash1) =
            decide_pull_action(&path, Some(&base_hash), remote).unwrap();
        assert_eq!(action1, PullAction::Conflict);
        let recorded1 = apply_pull_action(
            action1,
            &path,
            remote,
            remote_hash1,
            false,
            &p,
            "test",
            Some(&base_hash),
        )
        .unwrap();
        assert_eq!(recorded1, base_hash, "first pull preserves base");

        // Local and remote unchanged → second pull must still see a
        // conflict because the lockfile base never moved.
        let (action2, _) = decide_pull_action(&path, Some(&recorded1), remote).unwrap();
        assert_eq!(action2, PullAction::Conflict, "second pull must re-prompt");
        p.finish("test");
    }
}
