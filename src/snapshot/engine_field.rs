use crate::model::EngineField;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write an engine field as `<fields_dir>/<slug>.json`. `fields_dir` is
/// expected to be `engines/<engine_slug>/fields/`. Returns the bytes
/// written.
pub fn write_engine_field(dir: &Path, slug: &str, f: &EngineField) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = crate::snapshot::key_order::serialize_for_disk(f)
        .context("serializing engine field")?;
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_engine_field(dir: &Path, slug: &str) -> Result<EngineField> {
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

    fn sample() -> EngineField {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/engine_fields/1",
            "name": "F",
            "engine": "https://x/api/v1/engines/1"
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample();
        write_engine_field(dir.path(), "f", &original).unwrap();
        let read = read_engine_field(dir.path(), "f").unwrap();
        assert_eq!(original, read);
    }
}
