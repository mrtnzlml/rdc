use crate::model::Queue;
use anyhow::{Context, Result};
use std::path::Path;

/// Read a queue from disk: loads `<queue_dir>/queue.json`.
pub fn read_queue(queue_dir: &Path) -> Result<Queue> {
    let path = queue_dir.join("queue.json");
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let q: Queue =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(q)
}
