use crate::model::Inbox;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write an inbox's JSON to `<queue_dir>/inbox.json`. Returns the bytes written.
pub fn write_inbox(queue_dir: &Path, inbox: &Inbox) -> Result<Vec<u8>> {
    let path = queue_dir.join("inbox.json");
    let bytes = serde_json::to_vec_pretty(inbox)
        .context("serializing inbox")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

/// Read an inbox from disk: loads `<queue_dir>/inbox.json`.
pub fn read_inbox(queue_dir: &Path) -> Result<Inbox> {
    let path = queue_dir.join("inbox.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let inbox: Inbox = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(inbox)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> Inbox {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/inboxes/1",
            "name": "Inbox",
            "email": "x@mock",
            "queues": ["https://x/api/v1/queues/1"]
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("q1")).unwrap();
        let original = sample();
        write_inbox(&dir.path().join("q1"), &original).unwrap();
        let read = read_inbox(&dir.path().join("q1")).unwrap();
        assert_eq!(original, read);
    }
}
