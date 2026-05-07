use crate::model::Workflow;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a workflow as `<dir>/<slug>.json`. Returns the bytes written.
pub fn write_workflow(dir: &Path, slug: &str, w: &Workflow) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = serde_json::to_vec_pretty(w).context("serializing workflow")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_workflow(dir: &Path, slug: &str) -> Result<Workflow> {
    let path = dir.join(format!("{slug}.json"));
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> Workflow {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/workflows/1",
            "name": "W",
            "steps": []
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample();
        write_workflow(dir.path(), "w", &original).unwrap();
        let read = read_workflow(dir.path(), "w").unwrap();
        assert_eq!(original, read);
    }
}
