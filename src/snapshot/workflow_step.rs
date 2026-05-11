use crate::model::WorkflowStep;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a workflow step as `<steps_dir>/<slug>.json`. `steps_dir` is
/// expected to be `workflows/<workflow_slug>/steps/`. Returns the bytes
/// written.
pub fn write_workflow_step(dir: &Path, slug: &str, s: &WorkflowStep) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = serde_json::to_vec_pretty(s).context("serializing workflow step")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_workflow_step(dir: &Path, slug: &str) -> Result<WorkflowStep> {
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

    fn sample() -> WorkflowStep {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/workflow_steps/1",
            "name": "S",
            "workflow": "https://x/api/v1/workflows/1"
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample();
        write_workflow_step(dir.path(), "s", &original).unwrap();
        let read = read_workflow_step(dir.path(), "s").unwrap();
        assert_eq!(original, read);
    }
}
