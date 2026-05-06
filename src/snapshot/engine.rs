use crate::model::Engine;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write an engine as `<dir>/<slug>.json`. Returns the bytes written.
pub fn write_engine(dir: &Path, slug: &str, e: &Engine) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = serde_json::to_vec_pretty(e)
        .context("serializing engine")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_engine(dir: &Path, slug: &str) -> Result<Engine> {
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
        let original = sample();
        write_engine(dir.path(), "e", &original).unwrap();
        let read = read_engine(dir.path(), "e").unwrap();
        assert_eq!(original, read);
    }
}
