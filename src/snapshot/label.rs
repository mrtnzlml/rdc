use crate::model::Label;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a label as `<dir>/<slug>.json`. Returns the bytes written.
pub fn write_label(dir: &Path, slug: &str, l: &Label) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = crate::snapshot::key_order::serialize_for_disk(l)
        .context("serializing label")?;
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_label(dir: &Path, slug: &str) -> Result<Label> {
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

    fn sample() -> Label {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/labels/1",
            "name": "L",
            "organization": "https://x/api/v1/organizations/1"
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample();
        write_label(dir.path(), "l", &original).unwrap();
        let read = read_label(dir.path(), "l").unwrap();
        assert_eq!(original, read);
    }
}
