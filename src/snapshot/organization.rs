use crate::model::Organization;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write an organization to disk as a single JSON file at the given path.
/// (The file path is fixed to `<env_root>/organization.json` by the caller;
/// this codec is path-agnostic.)
pub fn write_organization(path: &Path, org: &Organization) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(org)
        .context("serializing organization")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(path, &bytes)?;
    Ok(())
}

/// Read an organization from disk.
pub fn read_organization(path: &Path) -> Result<Organization> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let org: Organization = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(org)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn sample() -> Organization {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/organizations/1",
            "name": "Acme",
            "modified_at": "2026-03-01T08:00:00Z"
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("organization.json");
        let original = sample();
        write_organization(&path, &original).unwrap();
        let read = read_organization(&path).unwrap();
        assert_eq!(original, read);
    }

    #[test]
    fn writes_pretty_json_with_trailing_newline() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("organization.json");
        write_organization(&path, &sample()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.ends_with('\n'));
        assert!(raw.contains("  \"id\": 1"), "expected pretty-printed JSON, got: {raw}");
    }
}
