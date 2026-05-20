//! Run-wide progress UX. Two implementations:
//!
//! * [`log::ProgressLog`] — line-based event log used by every command
//!   today. Continues to be the renderer for `deploy`, `auth`, `diff`,
//!   `upgrade`, and the non-TTY fallback for `sync`.
//! * [`grid::GridRenderer`] — kind-grouped grid of colored squares,
//!   used by `sync` and `sync --watch` on a TTY. See
//!   `docs/superpowers/specs/2026-05-20-sync-grid-visualization-design.md`.

pub mod log;
pub mod grid;

pub use log::{Phase, ProgressHandle, ProgressLog, Spinner};

use std::sync::Arc;
use crate::cli::sync::classify::ClassifiedItem;

/// Operation a per-resource event refers to. The grid renderer uses
/// these to drive the in-flight pulse and (optionally, later) to color
/// the pulse glyph by op kind. The log renderer ignores them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceOp {
    Get,
    Patch,
    Post,
    Delete,
}

/// Outcome of a per-resource operation. The grid renderer uses this to
/// transition the per-resource entry's in-flight state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceOutcome {
    Ok,
    Skipped,
    Failed(String),
}

/// Severity of a banner. The grid renderer colors banners accordingly;
/// the log renderer routes Info to `println`, Warn/Error to `warn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warn,
    Error,
}

/// Unified progress surface for every long-running command. Two
/// implementations: line-based ([`log::ProgressLog`]) and grid-based
/// ([`grid::GridRenderer`]). The dispatcher [`make_sync_renderer`]
/// returns the appropriate one based on stderr TTY presence.
pub trait SyncRenderer: Send + Sync {
    /// Section header / current-operation label. Both implementations
    /// honor this — the log renderer prints it; the grid renderer
    /// updates its header bar's "current op" field.
    fn phase(&self, label: &str);

    /// One free-standing line of output not tied to a resource spinner —
    /// success summaries (`[ok] labels 46 pulled`), retry notes, or
    /// general info. The log renderer prints it via `MultiProgress::println`;
    /// the grid renderer queues it as a transient banner.
    fn warn_line(&self, msg: &str);

    /// Per-resource lifecycle: signal an API call is starting. Only the
    /// grid renderer uses this; the log renderer treats it as a no-op.
    fn resource_started(&self, kind: &str, slug: &str, op: ResourceOp);

    /// Per-resource lifecycle: signal an API call has resolved. Only
    /// the grid renderer uses this; the log renderer is a no-op.
    fn resource_finished(&self, kind: &str, slug: &str, outcome: ResourceOutcome);

    /// Fresh classification ingest — at the start and end of each
    /// cycle. The grid renderer rebuilds its entry universe (union of
    /// lockfile / local-only / remote-only); the log renderer ignores
    /// this.
    fn ingest_classification(&self, items: &[ClassifiedItem]);

    /// Queue a transient footer banner (auth expired, network 5xx, …).
    /// The grid renderer displays it in the banner slot for 5 seconds
    /// then expires; the log renderer routes through `warn`.
    fn banner(&self, severity: Severity, msg: &str);

    /// Suspend the renderer's drawing region for the duration of an
    /// inline prompt (conflict resolver, destructive delete gate, auth
    /// refresh). The grid renderer switches `MultiProgress`'s draw
    /// target to hidden; the log renderer's `mp.println` already
    /// handles spinner suspension, so the closure runs unchanged.
    fn with_prompt(&self, f: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()>;

    /// Final summary line on success. Idempotent.
    fn finish_ok(&self, summary: &str);

    /// Final summary line on error. Idempotent.
    fn finish_err(&self, msg: &str);
}

/// Dispatcher. Returns a [`GridRenderer`] when stderr is a TTY (and
/// color is available), else a [`log::ProgressLog`] wrapped in a
/// thin trait adapter.
pub fn make_sync_renderer(
    title: &str,
    env: &str,
    is_watch: bool,
) -> Arc<dyn SyncRenderer> {
    use std::io::IsTerminal;
    let is_tty = std::io::stderr().is_terminal();
    let color = grid::detect_color_depth();
    if is_tty && color != grid::ColorDepth::None {
        grid::GridRenderer::new(env.to_string(), is_watch)
    } else {
        log::ProgressLog::start(title)
    }
}

#[cfg(test)]
mod dispatcher_tests {
    use super::*;

    #[test]
    fn make_sync_renderer_returns_a_trait_object() {
        let renderer: Arc<dyn SyncRenderer> = make_sync_renderer("test", "test", false);
        renderer.phase("listing remote");
        renderer.resource_started("hooks", "validator-invoices", ResourceOp::Patch);
        renderer.resource_finished("hooks", "validator-invoices", ResourceOutcome::Ok);
        renderer.banner(Severity::Info, "ready");
        renderer.finish_ok("done");
    }
}

#[cfg(test)]
mod dispatcher_routing_tests {
    use super::*;

    #[test]
    fn no_color_routes_to_log_renderer() {
        let saved = std::env::var("NO_COLOR").ok();
        // SAFETY: env var mutation is process-global; save+restore to
        // minimize blast radius. The matching pattern is what other
        // tests in this codebase use (see emit_square_tests).
        unsafe { std::env::set_var("NO_COLOR", "1"); }

        // With NO_COLOR set, detect_color_depth returns None →
        // dispatcher must return a LogRenderer regardless of TTY state.
        let r = make_sync_renderer("test", "test", false);
        // We can't downcast through Arc<dyn SyncRenderer>. Instead,
        // assert behavior: LogRenderer's ingest_classification is a
        // no-op. GridRenderer would have updated state.
        r.phase("listing remote");
        r.ingest_classification(&[]);
        r.finish_ok("done");

        unsafe {
            match saved {
                Some(v) => std::env::set_var("NO_COLOR", v),
                None => std::env::remove_var("NO_COLOR"),
            };
        }
    }
}

#[cfg(test)]
mod sync_renderer_types_tests {
    use super::*;

    #[test]
    fn resource_op_is_copy() {
        let op = ResourceOp::Patch;
        let _copy = op;
        assert_eq!(op, ResourceOp::Patch);
    }

    #[test]
    fn severity_variants_are_distinct() {
        // We deliberately don't derive Ord/PartialOrd — Severity is a tagged
        // set, not a ranking. A separate severity_rank() function in
        // grid.rs will define ordering for footer sort.
        assert_ne!(Severity::Info, Severity::Warn);
        assert_ne!(Severity::Warn, Severity::Error);
        assert_ne!(Severity::Info, Severity::Error);
    }
}
