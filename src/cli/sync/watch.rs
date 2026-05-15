//! `rdc sync --watch <env>` — foreground watch mode.
//!
//! Spec: docs/superpowers/specs/2026-05-14-watch-mode-design.md

use anyhow::Result;
use std::time::Duration;

pub async fn run_watch(
    _env: &str,
    _interactive: bool,
    _allow_deletes: bool,
    _no_push: bool,
    _no_pull: bool,
    _poll_interval: Option<Duration>,
    _verbose: bool,
) -> Result<()> {
    anyhow::bail!("watch mode not yet implemented");
}
