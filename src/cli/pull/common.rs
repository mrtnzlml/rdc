use crate::api::{ApiError, RossumClient};
use crate::paths::Paths;
use crate::progress::OverallProgress;
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
    progress: &Arc<OverallProgress>,
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
                progress.println(format!("warning: skipping {kind} — token lacks permission (403)"));
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
pub struct RemoteCatalog {
    pub organization: crate::model::Organization,
    pub workspaces: Vec<crate::model::Workspace>,
    pub queues: Vec<crate::model::Queue>,
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

/// Phase 1 of pull: list every kind from the env's API and accumulate the
/// progress bar's total denominator. No ticks happen here — Phase 2 (in
/// `pull::run_drivers`) or the sync classifier consumes the catalog.
///
/// Listing order is preserved exactly as it was inlined in `pull::run_drivers`
/// so cross-env diffs and bar pacing stay identical to the pre-refactor flow.
pub async fn list_remote(
    ctx: &mut PullCtx<'_>,
    env_cfg: &crate::config::EnvConfig,
    env: &str,
    token: &str,
    progress: &Arc<crate::progress::OverallProgress>,
) -> Result<RemoteCatalog> {
    let organization = crate::cli::pull::organization::list(ctx, env_cfg.org_id, progress).await
        .with_context(|| format!("listing organization for env '{env}'"))?;
    progress.inc_total(1);

    let workspaces = crate::cli::pull::workspaces::list(ctx, progress).await
        .with_context(|| format!("listing workspaces for env '{env}'"))?;
    progress.inc_total(workspaces.len() as u64);

    let queues = crate::cli::pull::queues::list(ctx, progress).await
        .with_context(|| format!("listing queues for env '{env}'"))?;
    progress.inc_total(queues.len() as u64);

    let hooks = crate::cli::pull::hooks::list(ctx, progress).await
        .with_context(|| format!("listing hooks for env '{env}'"))?;
    progress.inc_total(hooks.len() as u64);

    let rules = crate::cli::pull::rules::list(ctx, progress).await
        .with_context(|| format!("listing rules for env '{env}'"))?;
    progress.inc_total(rules.len() as u64);

    let labels = crate::cli::pull::labels::list(ctx, progress).await
        .with_context(|| format!("listing labels for env '{env}'"))?;
    progress.inc_total(labels.len() as u64);

    let engines = crate::cli::pull::engines::list(ctx, progress).await
        .with_context(|| format!("listing engines for env '{env}'"))?;
    progress.inc_total(engines.len() as u64);

    let engine_fields = crate::cli::pull::engine_fields::list(ctx, progress).await
        .with_context(|| format!("listing engine fields for env '{env}'"))?;
    progress.inc_total(engine_fields.len() as u64);

    let workflows = crate::cli::pull::workflows::list(ctx, progress).await
        .with_context(|| format!("listing workflows for env '{env}'"))?;
    progress.inc_total(workflows.len() as u64);

    let workflow_steps = crate::cli::pull::workflow_steps::list(ctx, progress).await
        .with_context(|| format!("listing workflow steps for env '{env}'"))?;
    progress.inc_total(workflow_steps.len() as u64);

    let email_templates = crate::cli::pull::email_templates::list(ctx, progress).await
        .with_context(|| format!("listing email templates for env '{env}'"))?;
    progress.inc_total(email_templates.len() as u64);

    let mdh = crate::cli::pull::mdh::list(env_cfg, token, progress).await
        .with_context(|| format!("listing MDH datasets for env '{env}'"))?;
    progress.inc_total(mdh.collections.len() as u64);

    Ok(RemoteCatalog {
        organization, workspaces, queues, hooks, rules, labels,
        engines, engine_fields, workflows, workflow_steps,
        email_templates, mdh,
    })
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
        ObjectEntry { id, url, modified_at, content_hash },
    );
}

/// Format `"<n> <noun>"` with correct singular/plural agreement.
/// Used by the pull summary line and any future count-aware UX.
pub fn pluralize(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
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
/// writes `<file>.<env>` next to the local file, keeps local, returns
/// the local hash.
pub fn apply_pull_action(
    action: PullAction,
    local_path: &Path,
    remote_bytes: &[u8],
    remote_hash: String,
    interactive: bool,
    progress: &Arc<OverallProgress>,
    env: &str,
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
                resolve_conflict_interactive(local_path, remote_bytes, &remote_hash, progress, env)
            } else {
                shadow_file_conflict(local_path, remote_bytes, progress, env)
            }
        }
    }
}

/// The legacy shadow-file behavior for a conflict: write
/// `<file>.<env>`, keep local on disk, return the local hash. Used when
/// `interactive == false` (CI/non-TTY/--yes) and as a fallback from the
/// resolver when the user picks `[s]kip`.
fn shadow_file_conflict(
    local_path: &Path,
    remote_bytes: &[u8],
    progress: &Arc<OverallProgress>,
    env: &str,
) -> Result<String> {
    use crate::snapshot::writer::write_atomic;
    let conflict_path = crate::paths::shadow_path_for(local_path, env);
    write_atomic(&conflict_path, remote_bytes)?;
    progress.println(format!(
        "warning: {} conflict — local preserved, remote at {}",
        local_path.display(),
        conflict_path.display(),
    ));
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
    progress: &Arc<OverallProgress>,
    env: &str,
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
        Resolution::Skip => shadow_file_conflict(local_path, remote_bytes, progress, env),
        Resolution::Abort => Err(anyhow::Error::new(PullAborted)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pluralize_singular() {
        assert_eq!(pluralize(1, "hook", "hooks"), "1 hook");
        assert_eq!(pluralize(1, "inbox", "inboxes"), "1 inbox");
    }

    #[test]
    fn pluralize_plural() {
        assert_eq!(pluralize(0, "hook", "hooks"), "0 hooks");
        assert_eq!(pluralize(2, "hook", "hooks"), "2 hooks");
        assert_eq!(pluralize(0, "inbox", "inboxes"), "0 inboxes");
    }

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
        let p = crate::progress::OverallProgress::start("test");
        let h = apply_pull_action(PullAction::Write, &path, b"hello", "h".repeat(64), false, &p, "test").unwrap();
        p.finish();
        assert_eq!(h, "h".repeat(64));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn apply_conflict_non_interactive_writes_remote_sibling() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.json");
        std::fs::write(&path, b"local").unwrap();
        // interactive=false → legacy shadow-file behavior.
        let p = crate::progress::OverallProgress::start("test");
        let _ = apply_pull_action(PullAction::Conflict, &path, b"remote", "h".repeat(64), false, &p, "test").unwrap();
        p.finish();
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
        let p = crate::progress::OverallProgress::start("test");
        let out = skip_on_permission_denied(err, "engines", &p).unwrap();
        p.finish();
        assert!(out.is_empty());
    }

    #[test]
    fn skip_on_permission_denied_propagates_other_errors() {
        let err: Result<Vec<u32>> = Err(anyhow!(ApiError::Status {
            status: 500,
            body: "boom".into()
        }));
        let p = crate::progress::OverallProgress::start("test");
        assert!(skip_on_permission_denied(err, "engines", &p).is_err());
    }

    #[test]
    fn skip_on_permission_denied_passes_through_ok() {
        let v: Result<Vec<u32>> = Ok(vec![1, 2, 3]);
        let p = crate::progress::OverallProgress::start("test");
        let out = skip_on_permission_denied(v, "engines", &p).unwrap();
        p.finish();
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
        let p = crate::progress::OverallProgress::start("test");
        let h = apply_pull_action(
            PullAction::NoChange,
            &path,
            b"different remote bytes",
            "h".repeat(64),
            false,
            &p,
            "test",
        )
        .unwrap();
        p.finish();
        assert_eq!(h, "h".repeat(64));
        // Local file unchanged byte-for-byte.
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
    }
}
