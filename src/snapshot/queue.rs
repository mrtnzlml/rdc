use crate::model::Queue;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a queue's JSON to `<queue_dir>/queue.json`. Returns the bytes written
/// (for content_hash). The caller is responsible for `queue_dir` existing.
pub fn write_queue(queue_dir: &Path, q: &Queue) -> Result<Vec<u8>> {
    let path = queue_dir.join("queue.json");
    let bytes = crate::snapshot::key_order::serialize_for_disk(q)
        .context("serializing queue")?;
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

/// Read a queue from disk: loads `<queue_dir>/queue.json`.
pub fn read_queue(queue_dir: &Path) -> Result<Queue> {
    let path = queue_dir.join("queue.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let q: Queue = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> Queue {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/queues/1",
            "name": "Q",
            "workspace": "https://x/api/v1/workspaces/1",
            "schema": "https://x/api/v1/schemas/1"
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q1")).unwrap();
        let original = sample();
        write_queue(&dir.path().join("q1"), &original).unwrap();
        let read = read_queue(&dir.path().join("q1")).unwrap();
        assert_eq!(original, read);
    }

    #[test]
    fn writes_into_queue_json_inside_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q1")).unwrap();
        write_queue(&dir.path().join("q1"), &sample()).unwrap();
        assert!(dir.path().join("q1/queue.json").exists());
    }
}
