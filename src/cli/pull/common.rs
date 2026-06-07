use crate::api::{ApiError, RossumClient};
use crate::log::{Action, Log};
use crate::paths::Paths;
use crate::state::{Lockfile, ObjectEntry, content_hash};
use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// If `result` is a 403 permission_denied from the Rossum API, log a warning
/// and return an empty list — the kind is unavailable to this token, but
/// other kinds should still pull. Otherwise propagate the error unchanged.
pub fn skip_on_permission_denied<T>(
    result: Result<Vec<T>>,
    kind: &str,
    progress: &Arc<Log>,
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
                progress.event(
                    Action::Skip,
                    &format!("{kind} (403 — token lacks permission)"),
                );
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
/// (queues fetching schema bodies by id, mdh fetching indexes + search-indexes).
/// The real throughput cap is the per-token `RateLimiter` (10 req/s); this
/// bound just limits how many requests are outstanding at once.
pub const PULL_FANOUT: usize = 5;

/// All kinds listed from one env's API. Produced by Phase 1 of pull,
/// consumed by Phase 2 (pull) or by the sync classifier.
///
/// `schemas_by_queue_id` is pre-fetched by `list_remote` because the Rossum
/// API's `/schemas` list omits `content` — each body must be fetched by id.
/// `inboxes_by_queue_id` is built from a single bulk `GET /inboxes` list.
/// Both maps are consumed by the sync classifier (for remote hashes) and by
/// [`crate::cli::pull::queues::process`] (for writing), so a single sync
/// cycle pays exactly one fetch per schema and zero extra inbox fetches.
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
/// Each list call emits a single `list <kind> (<N>)` event line after its
/// API call returns.
///
/// The top-level list calls run **concurrently** with a bounded
/// fan-out at [`PULL_FANOUT`] (the same cap that
/// [`prefetch_queue_schemas`] uses). The bound matches the existing
/// queue-schema prefetch pattern and avoids overwhelming the wire on
/// dense-config envs while still cutting wall-clock for a typical sync
/// roughly in half versus the prior fully-sequential pass. The `list`
/// lines commit in whatever order responses return, so the final
/// transcript may be reordered relative to the declaration order of
/// `Kind` below.
/// The per-queue schema prefetch runs serially after `queues` resolves
/// because it depends on the queue list. Inboxes are fetched concurrently
/// as part of the top-level kind set via a bulk `GET /inboxes`.
///
/// Each list-call future explicitly yields once before issuing the
/// request (`tokio::task::yield_now`). This matters under
/// `current_thread` runtimes where mocked-HTTP responses can return so
/// fast that the stream polls a batch of ready futures back-to-back
/// without surrendering the scheduler — that starves long-lived peers
/// such as the watch-mode poll-interval timer. The yield is harmless in
/// production (a no-op on a runtime that already cooperates over real
/// network I/O) and load-bearing for the test suite's watch-mode
/// timing assertions.
pub async fn list_remote(
    ctx: &mut PullCtx<'_>,
    env_cfg: &crate::config::EnvConfig,
    env: &str,
    token: &str,
    progress: &Arc<Log>,
) -> Result<RemoteCatalog> {
    use futures::stream::{StreamExt, TryStreamExt};

    // Single shared immutable reborrow so each concurrent future below
    // borrows from one place. The list drivers each take `&PullCtx`, so
    // multiple concurrent reads coexist cleanly via this reborrow.
    let ctx_ref: &PullCtx = &*ctx;

    // Tag each top-level fetch so the heterogeneous results can be
    // re-grouped after `buffer_unordered` joins them. The variant set
    // matches the typed bindings reconstructed below.
    enum Listed {
        Organization(crate::model::Organization),
        Workspaces(Vec<crate::model::Workspace>),
        Queues(Vec<crate::model::Queue>),
        Inboxes(Vec<crate::model::Inbox>),
        Hooks(Vec<crate::model::Hook>),
        Rules(Vec<crate::model::Rule>),
        Labels(Vec<crate::model::Label>),
        Engines(Vec<crate::model::Engine>),
        EngineFields(Vec<crate::model::EngineField>),
        Workflows(Vec<crate::model::Workflow>),
        WorkflowSteps(Vec<crate::model::WorkflowStep>),
        EmailTemplates(Vec<crate::model::EmailTemplate>),
        Mdh(crate::cli::pull::mdh::MdhListed),
    }

    #[derive(Clone, Copy)]
    enum Kind {
        Organization,
        Workspaces,
        Queues,
        Inboxes,
        Hooks,
        Rules,
        Labels,
        Engines,
        EngineFields,
        Workflows,
        WorkflowSteps,
        EmailTemplates,
        Mdh,
    }

    let kinds = [
        Kind::Organization,
        Kind::Workspaces,
        Kind::Queues,
        Kind::Inboxes,
        Kind::Hooks,
        Kind::Rules,
        Kind::Labels,
        Kind::Engines,
        Kind::EngineFields,
        Kind::Workflows,
        Kind::WorkflowSteps,
        Kind::EmailTemplates,
        Kind::Mdh,
    ];

    progress.start_phase(Action::List, "listing", 0);
    let results: Vec<Listed> = futures::stream::iter(kinds.iter().copied())
        .map(|kind| {
            async move {
                // Force a yield BEFORE each list call so the runtime can
                // service other tasks (e.g. the watch-mode poll timer)
                // between in-flight requests. Without this, a fast
                // mocked-HTTP environment can have the stream poll a
                // batch of ready futures back-to-back without ever
                // returning control to the runtime's scheduler.
                tokio::task::yield_now().await;
                match kind {
                    Kind::Organization => {
                        let r =
                            crate::cli::pull::organization::list(ctx_ref, env_cfg.org_id, progress)
                                .await
                                .with_context(|| format!("listing organization for env '{env}'"))?;
                        progress.event(Action::List, &format!("organization ({})", r.name));
                        anyhow::Ok(Listed::Organization(r))
                    }
                    Kind::Workspaces => {
                        let r = crate::cli::pull::workspaces::list(ctx_ref, progress)
                            .await
                            .with_context(|| format!("listing workspaces for env '{env}'"))?;
                        progress.event(Action::List, &format!("workspaces ({})", r.len()));
                        anyhow::Ok(Listed::Workspaces(r))
                    }
                    Kind::Queues => {
                        let r = crate::cli::pull::queues::list(ctx_ref, progress)
                            .await
                            .with_context(|| format!("listing queues for env '{env}'"))?;
                        progress.event(Action::List, &format!("queues ({})", r.len()));
                        anyhow::Ok(Listed::Queues(r))
                    }
                    Kind::Inboxes => {
                        let r = skip_on_permission_denied(
                            ctx_ref
                                .client
                                .list_inboxes(Some(progress.clone()))
                                .await
                                .with_context(|| format!("listing inboxes for env '{env}'")),
                            "inboxes",
                            progress,
                        )?;
                        progress.event(Action::List, &format!("inboxes ({})", r.len()));
                        anyhow::Ok(Listed::Inboxes(r))
                    }
                    Kind::Hooks => {
                        let r = crate::cli::pull::hooks::list(ctx_ref, progress)
                            .await
                            .with_context(|| format!("listing hooks for env '{env}'"))?;
                        progress.event(Action::List, &format!("hooks ({})", r.len()));
                        anyhow::Ok(Listed::Hooks(r))
                    }
                    Kind::Rules => {
                        let r = crate::cli::pull::rules::list(ctx_ref, progress)
                            .await
                            .with_context(|| format!("listing rules for env '{env}'"))?;
                        progress.event(Action::List, &format!("rules ({})", r.len()));
                        anyhow::Ok(Listed::Rules(r))
                    }
                    Kind::Labels => {
                        let r = crate::cli::pull::labels::list(ctx_ref, progress)
                            .await
                            .with_context(|| format!("listing labels for env '{env}'"))?;
                        progress.event(Action::List, &format!("labels ({})", r.len()));
                        anyhow::Ok(Listed::Labels(r))
                    }
                    Kind::Engines => {
                        let r = crate::cli::pull::engines::list(ctx_ref, progress)
                            .await
                            .with_context(|| format!("listing engines for env '{env}'"))?;
                        progress.event(Action::List, &format!("engines ({})", r.len()));
                        anyhow::Ok(Listed::Engines(r))
                    }
                    Kind::EngineFields => {
                        let r = crate::cli::pull::engine_fields::list(ctx_ref, progress)
                            .await
                            .with_context(|| format!("listing engine fields for env '{env}'"))?;
                        progress.event(Action::List, &format!("engine_fields ({})", r.len()));
                        anyhow::Ok(Listed::EngineFields(r))
                    }
                    Kind::Workflows => {
                        let r = crate::cli::pull::workflows::list(ctx_ref, progress)
                            .await
                            .with_context(|| format!("listing workflows for env '{env}'"))?;
                        progress.event(Action::List, &format!("workflows ({})", r.len()));
                        anyhow::Ok(Listed::Workflows(r))
                    }
                    Kind::WorkflowSteps => {
                        let r = crate::cli::pull::workflow_steps::list(ctx_ref, progress)
                            .await
                            .with_context(|| format!("listing workflow steps for env '{env}'"))?;
                        progress.event(Action::List, &format!("workflow_steps ({})", r.len()));
                        anyhow::Ok(Listed::WorkflowSteps(r))
                    }
                    Kind::EmailTemplates => {
                        let r = crate::cli::pull::email_templates::list(ctx_ref, progress)
                            .await
                            .with_context(|| format!("listing email templates for env '{env}'"))?;
                        progress.event(Action::List, &format!("email_templates ({})", r.len()));
                        anyhow::Ok(Listed::EmailTemplates(r))
                    }
                    Kind::Mdh => {
                        let r = crate::cli::pull::mdh::list(env_cfg, token, progress)
                            .await
                            .with_context(|| format!("listing MDH datasets for env '{env}'"))?;
                        progress.event(
                            Action::List,
                            &format!("mdh_datasets ({})", r.collections.len()),
                        );
                        anyhow::Ok(Listed::Mdh(r))
                    }
                }
            }
        })
        .buffer_unordered(PULL_FANOUT)
        .try_collect()
        .await?;
    progress.end_phase();

    // Re-group the results into typed bindings. Each variant appears
    // exactly once by construction.
    let mut organization: Option<crate::model::Organization> = None;
    let mut workspaces: Option<Vec<crate::model::Workspace>> = None;
    let mut queues: Option<Vec<crate::model::Queue>> = None;
    let mut inboxes: Option<Vec<crate::model::Inbox>> = None;
    let mut hooks: Option<Vec<crate::model::Hook>> = None;
    let mut rules: Option<Vec<crate::model::Rule>> = None;
    let mut labels: Option<Vec<crate::model::Label>> = None;
    let mut engines: Option<Vec<crate::model::Engine>> = None;
    let mut engine_fields: Option<Vec<crate::model::EngineField>> = None;
    let mut workflows: Option<Vec<crate::model::Workflow>> = None;
    let mut workflow_steps: Option<Vec<crate::model::WorkflowStep>> = None;
    let mut email_templates: Option<Vec<crate::model::EmailTemplate>> = None;
    let mut mdh: Option<crate::cli::pull::mdh::MdhListed> = None;
    for r in results {
        match r {
            Listed::Organization(v) => organization = Some(v),
            Listed::Workspaces(v) => workspaces = Some(v),
            Listed::Queues(v) => queues = Some(v),
            Listed::Inboxes(v) => inboxes = Some(v),
            Listed::Hooks(v) => hooks = Some(v),
            Listed::Rules(v) => rules = Some(v),
            Listed::Labels(v) => labels = Some(v),
            Listed::Engines(v) => engines = Some(v),
            Listed::EngineFields(v) => engine_fields = Some(v),
            Listed::Workflows(v) => workflows = Some(v),
            Listed::WorkflowSteps(v) => workflow_steps = Some(v),
            Listed::EmailTemplates(v) => email_templates = Some(v),
            Listed::Mdh(v) => mdh = Some(v),
        }
    }
    let organization = organization.expect("organization listed");
    let workspaces = workspaces.expect("workspaces listed");
    let queues = queues.expect("queues listed");
    let inboxes = inboxes.expect("inboxes listed");
    let hooks = hooks.expect("hooks listed");
    let rules = rules.expect("rules listed");
    let labels = labels.expect("labels listed");
    let engines = engines.expect("engines listed");
    let engine_fields = engine_fields.expect("engine_fields listed");
    let workflows = workflows.expect("workflows listed");
    let workflow_steps = workflow_steps.expect("workflow_steps listed");
    let email_templates = email_templates.expect("email_templates listed");
    let mdh = mdh.expect("mdh listed");
    let inboxes_by_queue_id = inboxes_by_queue(inboxes);

    // Per-queue schema prefetch. The `/schemas` list omits `content`, so
    // the body must be fetched by id. Doing it here gives the sync classifier
    // accurate remote hashes for queue-nested kinds, and the resulting map is
    // also handed to `pull::queues::process` so it can write the same bytes
    // without a second fetch round.
    //
    // Inboxes are now fetched via the bulk `/inboxes` list above and converted
    // to a queue-id map via `inboxes_by_queue`.
    let schemas_by_queue_id = prefetch_queue_schemas(ctx.client, &queues, progress)
        .await
        .with_context(|| format!("prefetching schemas for env '{env}'"))?;

    Ok(RemoteCatalog {
        organization,
        workspaces,
        queues,
        schemas_by_queue_id,
        inboxes_by_queue_id,
        hooks,
        rules,
        labels,
        engines,
        engine_fields,
        workflows,
        workflow_steps,
        email_templates,
        mdh,
    })
}

/// Per-queue schema body prefetch. The `/schemas` list omits `content`, so the
/// body must be fetched by id. Bounded fan-out at `PULL_FANOUT`.
async fn prefetch_queue_schemas(
    client: &RossumClient,
    queues: &[crate::model::Queue],
    progress: &Arc<Log>,
) -> Result<std::collections::BTreeMap<u64, crate::model::Schema>> {
    use futures::stream::{StreamExt, TryStreamExt};

    // Build (queue_id, queue_name, schema_id?) pairs once so the future
    // body doesn't borrow `queues`.
    let pairs: Vec<(u64, String, Option<u64>)> = queues
        .iter()
        .map(|q| {
            let s = q
                .schema
                .as_deref()
                .map(parse_id_from_url)
                .transpose()
                .ok()
                .flatten();
            (q.id, q.name.clone(), s)
        })
        .collect();
    let total = pairs.len();

    // Nothing to fetch — emit no summary, just return empty map.
    if total == 0 {
        return Ok(std::collections::BTreeMap::new());
    }

    let with_schema = pairs.iter().filter(|(_, _, sid)| sid.is_some()).count() as u64;
    // `List` (read), matching the `schemas (N fetched)` summary below and the
    // surrounding `list <kind>` lines — this fetch is a read sub-step of Phase 1.
    progress.start_phase(Action::List, "schema bodies", with_schema);

    let fetched_result: Result<Vec<(u64, Option<crate::model::Schema>)>> =
        futures::stream::iter(pairs)
            .map(|(qid, qname, sid)| {
                let progress = progress.clone();
                async move {
                    let schema = match sid {
                        Some(id) => {
                            let s = client
                                .get_schema(id, Some(progress.clone()))
                                .await
                                .with_context(|| {
                                    format!("prefetching schema {id} for queue '{qname}'")
                                })?;
                            progress.bump(1);
                            Some(s)
                        }
                        None => None,
                    };
                    Ok::<_, anyhow::Error>((qid, schema))
                }
            })
            .buffer_unordered(PULL_FANOUT)
            .try_collect()
            .await;

    let fetched = fetched_result?;
    progress.end_phase();

    let mut schemas = std::collections::BTreeMap::new();
    for (qid, s) in fetched {
        if let Some(s) = s {
            schemas.insert(qid, s);
        }
    }
    progress.event(
        Action::List,
        &format!("schemas ({} fetched)", schemas.len()),
    );
    Ok(schemas)
}

/// If `paths` is `Some` and non-empty, strip those overlay-managed dotted
/// paths from `bytes` (parse to Value, strip, re-serialize). Otherwise
/// return `bytes` unchanged. Used by every writable-kind pull driver to
/// keep the snapshot in its canonical pre-overlay form (spec §9.3).
pub fn maybe_strip_overlay(
    bytes: Vec<u8>,
    paths: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
) -> Result<Vec<u8>> {
    let Some(paths) = paths else {
        return Ok(bytes);
    };
    if paths.is_empty() {
        return Ok(bytes);
    }
    let mut value: serde_json::Value =
        serde_json::from_slice(&bytes).context("parsing JSON for overlay strip")?;
    crate::overlay::strip_paths(&mut value, paths);
    let mut out = serde_json::to_vec_pretty(&value).context("re-serializing post overlay strip")?;
    out.push(b'\n');
    Ok(out)
}

/// Record an object in the lockfile under the given kind/slug.
///
/// The object's URL is no longer stored — it is derived from `id` +
/// `api_base` (see [`crate::state::Lockfile::url_for_slug`]), so callers no
/// longer pass it.
pub fn record_object(
    lockfile: &mut Lockfile,
    kind: &str,
    slug: &str,
    id: u64,
    modified_at: Option<String>,
    content_hash: Option<String>,
) {
    lockfile.upsert(
        kind,
        slug,
        ObjectEntry {
            id,
            modified_at,
            content_hash,
            secrets_hash: None,
        },
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

/// Build the queue-id → inbox map from a bulk `/inboxes` list. Each inbox is
/// 1:1 with a queue (`inbox.queues[0]`). Inboxes whose queue URL is missing or
/// unparseable contribute nothing — same forgiving policy as the old per-queue
/// prefetch.
pub fn inboxes_by_queue(
    inboxes: Vec<crate::model::Inbox>,
) -> std::collections::BTreeMap<u64, crate::model::Inbox> {
    let mut map = std::collections::BTreeMap::new();
    for inbox in inboxes {
        if let Some(q_url) = inbox.queues.first()
            && let Ok(qid) = parse_id_from_url(q_url)
        {
            map.insert(qid, inbox);
        }
    }
    map
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

/// Rewrite portable-kind URLs in just-serialized object JSON to `rdc://<kind>/<slug>`
/// form using `lockfile`. Every pull driver runs this on its remote/proposed
/// JSON BEFORE the three-way merge + write, so the on-disk snapshot, the lockfile
/// baseline, and the remote candidate are all `rdc://`-native — the three-way
/// comparison then sees no spurious change and re-pulls are idempotent.
///
/// Refs whose target isn't in `lockfile` yet (cyclic/forward refs encountered
/// mid-pull, e.g. `hook.run_after`, `queue` ↔ `engine`/`inbox`) are left as URLs
/// here and finished by the `portabilize` post-pass once every slug is known; by
/// the next pull the persisted lockfile is complete, so this resolves them and
/// the merge stays Clean.
///
/// Key order and the trailing newline are preserved (only ref strings change),
/// and `content_hash` canonicalizes regardless, so written bytes stay stable.
pub(crate) fn portabilize_proposed(bytes: &[u8], lockfile: &Lockfile) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return bytes.to_vec();
    };
    crate::snapshot::refs::portabilize_value(&mut value, lockfile);
    let Ok(mut out) = serde_json::to_vec_pretty(&value) else {
        return bytes.to_vec();
    };
    if bytes.last() == Some(&b'\n') {
        out.push(b'\n');
    }
    out
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
    let remote_hash = content_hash(remote_bytes, &Lockfile::default());

    let Some(base) = base_hash else {
        return Ok((PullAction::Write, remote_hash));
    };

    if !local_path.exists() {
        return Ok((PullAction::Write, remote_hash));
    }

    let local_bytes =
        std::fs::read(local_path).with_context(|| format!("reading {}", local_path.display()))?;
    let local_hash = content_hash(&local_bytes, &Lockfile::default());

    // Short-circuit: canonicalized local == canonicalized remote means
    // any difference is noise (modifier etc.). Don't rewrite the file —
    // unless the on-disk format is stale (still carries a field that
    // the current serializer strips, e.g. `modified_at`). In that case
    // force a one-time rewrite so the on-disk layout catches up.
    if local_hash == remote_hash {
        if crate::snapshot::key_order::contains_hidden_fields(&local_bytes) {
            return Ok((PullAction::Write, remote_hash));
        }
        return Ok((PullAction::NoChange, remote_hash));
    }

    // Format-migration safety for UNEDITED legacy snapshots:
    //
    // When rdc is upgraded to a version that changes the canonical on-disk
    // form (e.g. engines `agenda_id` → sentinel, hooks `status` → sentinel,
    // `modified_at` stripped), a user's existing disk file is in the OLD
    // form and the lockfile `base_hash` was recorded from those OLD bytes.
    // If the file is UNEDITED relative to that base, `local_hash == base`
    // (because `content_hash` / `canonicalize_for_hash` already strips
    // `modified_at` as a noise field — the stored hash and the live hash
    // over the old bytes are equal). The branch below classifies this as
    // `PullAction::Write`, which silently rewrites the file to the new
    // codec form and advances the lockfile hash. No self-heal code is
    // needed: the normal three-way logic handles this case correctly.
    //
    // For a LOCALLY-EDITED legacy snapshot (`local_hash != base`), both
    // sides have diverged (local edit + codec-format migration from
    // remote), so the action is `Conflict` — the user's real edits must
    // be inspected manually.
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
    progress: &Arc<Log>,
    env: &str,
    base_hash: Option<&str>,
    paths: Option<&crate::paths::Paths>,
) -> Result<String> {
    use crate::snapshot::writer::write_atomic;
    match action {
        PullAction::Write => {
            write_atomic(local_path, remote_bytes)?;
            // Mirror to base cache: same bytes are now both the disk
            // truth AND the new merge base for the next sync.
            if let Some(p) = paths {
                crate::state::base_cache::write(p, local_path, remote_bytes)?;
            }
            Ok(remote_hash)
        }
        PullAction::KeepLocal => {
            let local_bytes = std::fs::read(local_path)
                .with_context(|| format!("reading {}", local_path.display()))?;
            // Local won the merge → it's the new base.
            if let Some(p) = paths {
                crate::state::base_cache::write(p, local_path, &local_bytes)?;
            }
            Ok(content_hash(&local_bytes, &Lockfile::default()))
        }
        PullAction::NoChange => {
            // Local and remote canonicalize equal — preserve disk bytes.
            // Hash is identical to remote_hash by construction. Cache
            // the disk bytes (whatever flavour the canonical form took)
            // so the next merge has them.
            if let Some(p) = paths
                && let Ok(on_disk) = std::fs::read(local_path)
            {
                crate::state::base_cache::write(p, local_path, &on_disk)?;
            }
            Ok(remote_hash)
        }
        PullAction::Conflict => {
            let resolved_hash = if interactive {
                resolve_conflict_interactive(
                    local_path,
                    remote_bytes,
                    &remote_hash,
                    progress,
                    env,
                    base_hash,
                )?
            } else {
                shadow_file_conflict(local_path, remote_bytes, progress, env, base_hash)?
            };
            // Only update the cache when the lockfile entry actually
            // advanced. Shadow-skip preserves `base_hash`, leaving the
            // prior cache (which holds true base bytes) untouched —
            // crucial, because the disk still holds LOCAL, not base.
            if let Some(p) = paths
                && base_hash != Some(resolved_hash.as_str())
                && let Ok(on_disk) = std::fs::read(local_path)
            {
                crate::state::base_cache::write(p, local_path, &on_disk)?;
            }
            Ok(resolved_hash)
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
    progress: &Arc<Log>,
    env: &str,
    base_hash: Option<&str>,
) -> Result<String> {
    use crate::snapshot::writer::write_atomic;
    let conflict_path = crate::paths::shadow_path_for(local_path, env);
    write_atomic(&conflict_path, remote_bytes)?;
    progress.event(Action::Warn, &format!(
        "{} conflict: local preserved, remote at {} (lockfile base preserved; re-run to resolve)",
        local_path.display(),
        conflict_path.display(),
    ));
    if let Some(prior) = base_hash {
        return Ok(prior.to_string());
    }
    // Defensive fallback: no prior base recorded (shouldn't happen for a
    // real conflict; conflicts presuppose `(local != base, remote != base)`).
    // Use local hash so the lockfile still gets a sensible entry.
    let local_bytes =
        std::fs::read(local_path).with_context(|| format!("reading {}", local_path.display()))?;
    Ok(content_hash(&local_bytes, &Lockfile::default()))
}

/// Drive the spec §8.3 resolver TUI on stdin/stderr. On
/// [`crate::cli::resolve::Resolution::Abort`] this returns a
/// [`crate::cli::resolve::PullAborted`]-wrapping anyhow error so the
/// pull runner can downcast and skip lockfile.save().
fn resolve_conflict_interactive(
    local_path: &Path,
    remote_bytes: &[u8],
    remote_hash: &str,
    progress: &Arc<Log>,
    env: &str,
    base_hash: Option<&str>,
) -> Result<String> {
    use crate::cli::resolve::{PullAborted, Resolution, prompt_resolve};
    use crate::snapshot::writer::write_atomic;

    // Read prompt input via the stdin coordinator (not `stdin().lock()`):
    // under `rdc sync --watch` the Enter-trigger reader owns stdin and
    // feeds prompts through the coordinator, so locking stdin here would
    // deadlock. Outside watch the coordinator reads stdin directly. This
    // path is normally pre-handled by the sync conflict resolver, but a
    // mid-cycle drift can still surface it.
    let stderr = std::io::stderr();
    let resolution = prompt_resolve(
        crate::cli::stdin_coord::CoordinatorStdin::new(),
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
            Ok(content_hash(&local_bytes, &Lockfile::default()))
        }
        Resolution::KeepRemote => {
            write_atomic(local_path, remote_bytes)?;
            Ok(remote_hash.to_string())
        }
        Resolution::Edit(edited) => {
            write_atomic(local_path, &edited)?;
            Ok(content_hash(&edited, &Lockfile::default()))
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
                progress.event(Action::Warn, &format!(
                    "{} partially resolved (markers retained); lockfile base preserved; re-run to resolve",
                    local_path.display(),
                ));
                Ok(prior.to_string())
            } else {
                Ok(content_hash(&edited, &Lockfile::default()))
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
    fn inboxes_by_queue_maps_by_attached_queue_id() {
        let inbox: crate::model::Inbox = serde_json::from_value(serde_json::json!({
            "id": 5, "url": "https://x/api/v1/inboxes/5", "name": "n", "email": "e",
            "queues": ["https://x/api/v1/queues/42"]
        }))
        .unwrap();
        let map = inboxes_by_queue(vec![inbox]);
        assert_eq!(map.get(&42).map(|i| i.id), Some(5));
    }

    #[test]
    fn inboxes_by_queue_skips_inbox_with_no_queue() {
        let inbox: crate::model::Inbox = serde_json::from_value(serde_json::json!({
            "id": 6, "url": "https://x/api/v1/inboxes/6", "name": "n", "email": "e",
            "queues": []
        }))
        .unwrap();
        assert!(inboxes_by_queue(vec![inbox]).is_empty());
    }

    #[test]
    fn parse_id_basic() {
        assert_eq!(
            parse_id_from_url("https://x/api/v1/schemas/1234").unwrap(),
            1234
        );
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
        let base = content_hash(remote, &Lockfile::default());
        let (action, _hash) = decide_pull_action(&path, Some(&base), remote).unwrap();
        assert_eq!(action, PullAction::KeepLocal);
    }

    #[test]
    fn write_when_only_remote_changed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        let original = b"{ \"original\": true }";
        std::fs::write(&path, original).unwrap();
        let base = content_hash(original, &Lockfile::default());
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
        let p = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let h = apply_pull_action(
            PullAction::Write,
            &path,
            b"hello",
            "h".repeat(64),
            false,
            &p,
            "test",
            None,
            None,
        )
        .unwrap();
        assert_eq!(h, "h".repeat(64));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn apply_conflict_non_interactive_writes_remote_sibling() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local").unwrap();
        // interactive=false → legacy shadow-file behavior.
        let p = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let _ = apply_pull_action(
            PullAction::Conflict,
            &path,
            b"remote",
            "h".repeat(64),
            false,
            &p,
            "test",
            None,
            None,
        )
        .unwrap();
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
            body: "permission_denied".into(),
            env: None,
        }));
        let p = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let out = skip_on_permission_denied(err, "engines", &p).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn skip_on_permission_denied_propagates_other_errors() {
        let err: Result<Vec<u32>> = Err(anyhow!(ApiError::Status {
            status: 500,
            body: "boom".into(),
            env: None,
        }));
        let p = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        assert!(skip_on_permission_denied(err, "engines", &p).is_err());
    }

    #[test]
    fn skip_on_permission_denied_passes_through_ok() {
        let v: Result<Vec<u32>> = Ok(vec![1, 2, 3]);
        let p = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let out = skip_on_permission_denied(v, "engines", &p).unwrap();
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn decide_forces_write_when_local_still_carries_modified_at() {
        // Migration: local on-disk JSON is in the legacy format (carries
        // `modified_at`). The new serializer strips it, so even though
        // canonical hashes still match, we must rewrite once to bring
        // the on-disk file in line.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"{\"name\":\"x\",\"modified_at\":\"t1\"}").unwrap();
        // Remote has modified_at = t2 (newer); same other content
        let remote = b"{\"name\":\"x\",\"modified_at\":\"t2\"}";
        // Base hash matches both (canonical strips modified_at)
        let base = content_hash(remote, &Lockfile::default());
        let (action, _hash) = decide_pull_action(&path, Some(&base), remote).unwrap();
        assert_eq!(action, PullAction::Write);
    }

    #[test]
    fn decide_returns_nochange_when_canonical_matches_and_no_hidden_fields_on_disk() {
        // Steady state: on-disk already lacks modified_at (e.g. after
        // the migration write happened on a prior pull). Canonical
        // hashes still match because content_hash strips noise.
        // No rewrite needed.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"{\"name\":\"x\"}").unwrap();
        let remote = b"{\"name\":\"x\",\"modified_at\":\"t2\"}";
        let base = content_hash(remote, &Lockfile::default());
        let (action, _hash) = decide_pull_action(&path, Some(&base), remote).unwrap();
        assert_eq!(action, PullAction::NoChange);
    }

    #[test]
    fn apply_nochange_does_not_modify_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"original").unwrap();
        let original_bytes = std::fs::read(&path).unwrap();
        let p = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let h = apply_pull_action(
            PullAction::NoChange,
            &path,
            b"different remote bytes",
            "h".repeat(64),
            false,
            &p,
            "test",
            None,
            None,
        )
        .unwrap();
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
        let p = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let recorded = apply_pull_action(
            PullAction::Conflict,
            &path,
            remote,
            content_hash(remote, &Lockfile::default()),
            false, // non-interactive → shadow_file_conflict
            &p,
            "test",
            Some(&prior_base),
            None,
        )
        .unwrap();
        // Lockfile must NOT advance — recorded hash equals prior base.
        assert_eq!(
            recorded, prior_base,
            "shadow-skip must preserve lockfile base"
        );
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
        let p = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);
        let recorded = apply_pull_action(
            PullAction::Conflict,
            &path,
            remote,
            content_hash(remote, &Lockfile::default()),
            false,
            &p,
            "test",
            None, // no prior base
            None,
        )
        .unwrap();
        assert_eq!(recorded, content_hash(local, &Lockfile::default()));
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
        let p = crate::log::Log::new(crate::cli::resolve::ColorMode::Plain);

        // First pull → conflict → shadow-skip.
        let (action1, remote_hash1) = decide_pull_action(&path, Some(&base_hash), remote).unwrap();
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
            None,
        )
        .unwrap();
        assert_eq!(recorded1, base_hash, "first pull preserves base");

        // Local and remote unchanged → second pull must still see a
        // conflict because the lockfile base never moved.
        let (action2, _) = decide_pull_action(&path, Some(&recorded1), remote).unwrap();
        assert_eq!(action2, PullAction::Conflict, "second pull must re-prompt");
        let _ = p; // Log dropped normally; no finish call needed.
    }
}
