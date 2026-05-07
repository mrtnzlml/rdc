use crate::model::IndexSet;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a collection's indexes to `<dataset_dir>/indexes.json`.
/// Returns the bytes written.
pub fn write_index_set(dataset_dir: &Path, s: &IndexSet) -> Result<Vec<u8>> {
    let path = dataset_dir.join("indexes.json");
    let bytes = serde_json::to_vec_pretty(s).context("serializing index set")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_index_set(dataset_dir: &Path) -> Result<IndexSet> {
    let path = dataset_dir.join("indexes.json");
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

    fn sample() -> IndexSet {
        let v = json!({
            "regular": [{ "name": "ix_id", "key": { "id": 1 } }],
            "search": []
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("vendors")).unwrap();
        let original = sample();
        write_index_set(&dir.path().join("vendors"), &original).unwrap();
        let read = read_index_set(&dir.path().join("vendors")).unwrap();
        assert_eq!(original, read);
    }
}
