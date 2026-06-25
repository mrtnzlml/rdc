use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Normalized, run-agnostic snapshot of the facts a scenario pins.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CapturedState {
    /// kind -> sorted lockfile slugs, with the run-id prefix stripped.
    #[serde(default)]
    pub lockfile_keys: BTreeMap<String, Vec<String>>,
    /// label -> portable ref value found on disk (run-id stripped).
    #[serde(default)]
    pub refs: BTreeMap<String, String>,
}

/// Returns true when the harness should WRITE golden files instead of
/// asserting against them (first capture / intentional re-baseline).
#[allow(dead_code)]
pub fn capture_mode() -> bool {
    std::env::var("RDC_LIVE_CAPTURE").map(|v| v == "1").unwrap_or(false)
}

/// Compare `actual` to the golden file at `path`. In capture mode, write
/// `actual` and pass (the maintainer reviews the diff before committing).
#[allow(dead_code)]
pub fn load_or_compare(path: &Path, actual: &CapturedState) -> Result<()> {
    if capture_mode() {
        let toml = toml::to_string_pretty(actual).context("serializing captured state")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(path, toml).with_context(|| format!("writing golden {}", path.display()))?;
        eprintln!("CAPTURED golden state -> {} (review before committing)", path.display());
        return Ok(());
    }
    let raw = std::fs::read_to_string(path).with_context(|| {
        format!(
            "missing golden {}. Run once with RDC_LIVE_CAPTURE=1 to create it, then review.",
            path.display()
        )
    })?;
    let expected: CapturedState = toml::from_str(&raw)?;
    if &expected != actual {
        bail!(
            "captured state mismatch for {}\nexpected: {:#?}\nactual:   {:#?}",
            path.display(),
            expected,
            actual
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::config::env_lock;
    use tempfile::TempDir;

    #[test]
    fn round_trips_via_toml() {
        let _g = env_lock();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("expected/x.toml");
        let mut st = CapturedState::default();
        st.lockfile_keys.insert("queues".into(), vec!["a".into(), "b".into()]);
        st.refs.insert("q.schema".into(), "rdc://schemas/a".into());
        // capture
        // SAFETY: serialized by env_lock; restored below.
        unsafe {
            std::env::set_var("RDC_LIVE_CAPTURE", "1");
        }
        load_or_compare(&path, &st).unwrap();
        unsafe {
            std::env::remove_var("RDC_LIVE_CAPTURE");
        }
        // compare equal
        load_or_compare(&path, &st).unwrap();
        // compare unequal
        let mut other = CapturedState::default();
        other.lockfile_keys.insert("queues".into(), vec!["a".into()]);
        assert!(load_or_compare(&path, &other).is_err());
    }
}
