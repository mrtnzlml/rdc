use crate::model::Engine;
use anyhow::{Context, Result};
use std::path::Path;

pub fn read_engine(engine_dir: &Path) -> Result<Engine> {
    let path = engine_dir.join("engine.json");
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}
