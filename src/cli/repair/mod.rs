//! `rdc repair <env>` — bring the local snapshot back into a clean state.
//!
//! Two modes today, one mandatory:
//!
//! * `--rebuild-lock` (online): back up the existing lockfile and
//!   re-pull everything. Local edits LOST.
//! * `--rename-slugs` (offline): rename local files whose slug no
//!   longer matches their JSON `name`. Cascade-aware. No API calls.
//!
//! Task 4 will add `--fix-store-anomaly` here.

pub mod rebuild_lock;
pub mod rename_slugs;

use anyhow::{anyhow, Result};

pub async fn run(
    env: &str,
    rebuild_lock: bool,
    rename_slugs: bool,
    check: bool,
    yes: bool,
) -> Result<()> {
    // Pick exactly one mode. No implicit default because both modes
    // touch on-disk files in irreversible ways.
    match (rebuild_lock, rename_slugs) {
        (false, false) => Err(anyhow!(
            "rdc repair needs a mode flag: --rebuild-lock or --rename-slugs"
        )),
        (true, true) => Err(anyhow!(
            "rdc repair --rebuild-lock and --rename-slugs are mutually exclusive"
        )),
        (true, false) => {
            if check {
                return Err(anyhow!(
                    "rdc repair --rebuild-lock does not support --check (it always re-pulls). \
                     Use git to preview what a rebuild would overwrite."
                ));
            }
            rebuild_lock::run(env).await
        }
        (false, true) => rename_slugs::run(env, check, yes).await,
    }
}
