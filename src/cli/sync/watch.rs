//! `rdc sync --watch <env>` — foreground watch mode.
//!
//! Spec: docs/superpowers/specs/2026-05-14-watch-mode-design.md

use anyhow::Result;
use std::time::Duration;

pub async fn run_watch(
    env: &str,
    _interactive: bool,
    _allow_deletes: bool,
    _no_push: bool,
    _no_pull: bool,
    poll_interval: Option<Duration>,
    _verbose: bool,
) -> Result<()> {
    eprintln!("watching envs/{env}/ ...");
    if let Some(d) = poll_interval {
        eprintln!("polling {env} every {}s ...", d.as_secs());
    } else {
        eprintln!("polling disabled");
    }
    tokio::signal::ctrl_c().await?;
    eprintln!("\nstopping watch.");
    Ok(())
}
