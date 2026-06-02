use crate::model::EngineField;
use anyhow::{Context, Result};
use std::path::Path;

pub fn read_engine_field(dir: &Path, slug: &str) -> Result<EngineField> {
    let path = dir.join(format!("{slug}.json"));
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}
