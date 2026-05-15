//! `rdc sync --watch <env>` — foreground watch mode.
//!
//! Spec: docs/superpowers/specs/2026-05-14-watch-mode-design.md

use anyhow::Result;
use std::time::Duration;

pub async fn run_watch(
    env: &str,
    interactive: bool,
    allow_deletes: bool,
    no_push: bool,
    no_pull: bool,
    poll_interval: Option<Duration>,
    _verbose: bool,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let paths = crate::paths::Paths::for_env(&cwd, env);

    // Initial reconcile.
    {
        let _lock = crate::cli::sync::lock::EnvLock::acquire(
            &paths.env_lock(),
            Duration::from_secs(30),
        )?;
        crate::cli::sync::run_cycle(
            env,
            interactive,
            false,
            false,
            allow_deletes,
            no_push,
            no_pull,
            true,
        )
        .await?;
    }

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
