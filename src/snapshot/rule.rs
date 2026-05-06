use crate::model::Rule;
use crate::snapshot::writer::write_atomic;
use anyhow::{Context, Result};
use std::path::Path;

/// Write a rule as `<dir>/<slug>.json`. Returns the bytes written.
pub fn write_rule(dir: &Path, slug: &str, r: &Rule) -> Result<Vec<u8>> {
    let path = dir.join(format!("{slug}.json"));
    let bytes = serde_json::to_vec_pretty(r)
        .context("serializing rule")?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(bytes)
}

pub fn read_rule(dir: &Path, slug: &str) -> Result<Rule> {
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

    fn sample() -> Rule {
        let v = json!({
            "id": 1,
            "url": "https://x/api/v1/rules/1",
            "name": "R",
            "queues": []
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn round_trip() {
        let dir = TempDir::new().unwrap();
        let original = sample();
        write_rule(dir.path(), "r", &original).unwrap();
        let read = read_rule(dir.path(), "r").unwrap();
        assert_eq!(original, read);
    }
}
