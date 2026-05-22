use crate::model::Collection;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a collection's metadata to `<dataset_dir>/collection.json`.
/// Returns the bytes written.
pub fn write_collection(dataset_dir: &Path, c: &Collection) -> Result<Vec<u8>> {
    let path = dataset_dir.join("collection.json");
    let bytes = crate::snapshot::key_order::serialize_for_disk(c)
        .context("serializing collection")?;
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_collection(dataset_dir: &Path) -> Result<Collection> {
    let path = dataset_dir.join("collection.json");
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

    fn sample() -> Collection {
        let v = json!({ "name": "vendors" });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("vendors")).unwrap();
        let original = sample();
        write_collection(&dir.path().join("vendors"), &original).unwrap();
        let read = read_collection(&dir.path().join("vendors")).unwrap();
        assert_eq!(original, read);
    }
}
