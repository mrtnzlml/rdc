//! Run-wide progress UX. Two implementations:
//!
//! * [`log::ProgressLog`] — line-based event log used by every command
//!   today. Continues to be the renderer for `deploy`, `auth`, `diff`,
//!   `upgrade`, and the non-TTY fallback for `sync`.
//! * [`grid::GridRenderer`] — kind-grouped grid of colored squares,
//!   used by `sync` and `sync --watch` on a TTY. See
//!   `docs/superpowers/specs/2026-05-20-sync-grid-visualization-design.md`.

pub mod log;
// pub mod grid; — added in Task 4.

pub use log::{Phase, ProgressHandle, ProgressLog, Spinner};
