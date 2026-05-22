use crate::model::Engine;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write an engine as `<engine_dir>/engine.json`. The engine dir is
/// expected to be `engines/<slug>/`; the engine's own JSON sits beside
/// the `fields/` subdir that contains its engine fields. Mirrors the
/// `workspaces/<ws>/workspace.json` pattern.
pub fn write_engine(engine_dir: &Path, e: &Engine) -> Result<Vec<u8>> {
    let path = engine_dir.join("engine.json");
    let bytes = crate::snapshot::key_order::serialize_for_disk(e)
        .context("serializing engine")?;
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_engine(engine_dir: &Path) -> Result<Engine> {
    let path = engine_dir.join("engine.json");
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

    fn sample() -> Engine {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/engines/1",
            "name": "E"
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let engine_dir = dir.path().join("e");
        std::fs::create_dir_all(&engine_dir).unwrap();
        let original = sample();
        write_engine(&engine_dir, &original).unwrap();
        let read = read_engine(&engine_dir).unwrap();
        assert_eq!(original, read);
    }
}
