//! Execute the classified plan. Filled in across subsequent tasks
//! (per-class dispatch: pull-side writes, push-side writes,
//! both-diverged resolver, remote-delete + double-conflicts).
//!
//! Spec: docs/superpowers/specs/2026-05-14-unified-sync-design.md.

use anyhow::Result;

/// Stub executor invoked from `sync::run` after the plan is rendered and
/// (when interactive) confirmed.
///
/// TODO(sync-impl): per-class dispatch — pull-side writes, push-side
/// writes, both-diverged resolver, remote-delete + double-conflicts.
/// Today this is a no-op so the clean-env smoke test exercises the full
/// pipeline (`list_remote` → `scan` → `classify` → `plan` → `confirm` →
/// `execute`) without any actual writes.
pub async fn run(
    _ctx: &mut crate::cli::pull::common::PullCtx<'_>,
    _catalog: &crate::cli::pull::common::RemoteCatalog,
    _classified: &[crate::cli::sync::classify::ClassifiedItem],
    _no_push: bool,
    _no_pull: bool,
    _interactive: bool,
    _progress: &std::sync::Arc<crate::progress::OverallProgress>,
) -> Result<()> {
    Ok(())
}
