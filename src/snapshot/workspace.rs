use crate::model::Workspace;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a workspace's metadata to `<workspace_dir>/workspace.json`.
/// The caller is responsible for `workspace_dir` existing.
pub fn write_workspace(workspace_dir: &Path, ws: &Workspace) -> Result<Vec<u8>> {
    let path = workspace_dir.join("workspace.json");
    let bytes = crate::snapshot::key_order::serialize_for_disk(ws)
        .context("serializing workspace")?;
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

/// Read a workspace from disk: loads `<workspace_dir>/workspace.json`.
pub fn read_workspace(workspace_dir: &Path) -> Result<Workspace> {
    let path = workspace_dir.join("workspace.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let ws: Workspace = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(ws)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> Workspace {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/workspaces/1",
            "name": "AP",
            "organization": "https://x/api/v1/organizations/1",
            "queues": []
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("ap")).unwrap();
        let original = sample();
        write_workspace(&dir.path().join("ap"), &original).unwrap();
        let read = read_workspace(&dir.path().join("ap")).unwrap();
        assert_eq!(original, read);
    }

    #[test]
    fn writes_into_workspace_json_inside_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("ap")).unwrap();
        write_workspace(&dir.path().join("ap"), &sample()).unwrap();
        assert!(dir.path().join("ap/workspace.json").exists());
    }
}
