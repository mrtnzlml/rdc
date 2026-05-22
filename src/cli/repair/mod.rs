//! `rdc repair <env>` — bring the local snapshot back into a clean state.
//!
//! Three modes, one mandatory:
//!
//! * `--rebuild-lock` (online): back up the existing lockfile and
//!   re-pull everything. Local edits LOST.
//! * `--rename-slugs` (offline): rename local files whose slug no
//!   longer matches their JSON `name`. Cascade-aware. No API calls.
//! * `--fix-store-anomaly` (online, interactive): repair hooks with
//!   `extension_source: "rossum_store"` and `hook_template: null`.

pub mod rebuild_lock;
pub mod rename_slugs;
pub mod store_anomaly;

use anyhow::{anyhow, Result};

pub async fn run(
    env: &str,
    rebuild_lock: bool,
    rename_slugs: bool,
    fix_store_anomaly: bool,
    check: bool,
    yes: bool,
) -> Result<()> {
    match (rebuild_lock, rename_slugs, fix_store_anomaly) {
        (false, false, false) => Err(anyhow!(
            "rdc repair needs a mode flag: --rebuild-lock, --rename-slugs, or --fix-store-anomaly"
        )),
        (true, false, false) => {
            if check {
                return Err(anyhow!(
                    "rdc repair --rebuild-lock does not support --check (it always re-pulls). \
                     Use git to preview what a rebuild would overwrite."
                ));
            }
            rebuild_lock::run(env).await
        }
        (false, true, false) => rename_slugs::run(env, check, yes).await,
        (false, false, true) => store_anomaly::run(env, check, yes).await,
        _ => Err(anyhow!(
            "repair mode flags are mutually exclusive; pick one"
        )),
    }
}
