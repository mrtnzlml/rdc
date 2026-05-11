use crate::model::Workflow;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a workflow as `<workflow_dir>/workflow.json`. Mirrors the
/// workspaces/<ws>/workspace.json pattern: the workflow's own JSON sits
/// beside the `steps/` subdir that contains its workflow steps.
pub fn write_workflow(workflow_dir: &Path, w: &Workflow) -> Result<Vec<u8>> {
    let path = workflow_dir.join("workflow.json");
    let mut bytes = serde_json::to_vec_pretty(w).context("serializing workflow")?;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_workflow(workflow_dir: &Path) -> Result<Workflow> {
    let path = workflow_dir.join("workflow.json");
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
        let wf_dir = dir.path().join("w");
        std::fs::create_dir_all(&wf_dir).unwrap();
        let original = sample();
        write_workflow(&wf_dir, &original).unwrap();
        let read = read_workflow(&wf_dir).unwrap();
        assert_eq!(original, read);
    }
}
