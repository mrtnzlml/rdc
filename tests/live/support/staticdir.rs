use crate::support::manifest::Manifest;
use anyhow::{Context, Result};
use std::path::PathBuf;

#[allow(dead_code)]
pub fn static_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/live")
}

#[allow(dead_code)]
pub fn load_manifest() -> Result<Manifest> {
    let path = static_dir().join("manifest.toml");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    Manifest::parse(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hermetic: validates the static folder is internally consistent without
    // any network — every manifest body file exists and topo-order succeeds.
    #[test]
    fn static_folder_is_consistent() {
        let m = load_manifest().expect("manifest parses");
        let dir = static_dir();
        for o in &m.objects {
            assert!(
                dir.join(&o.body).exists(),
                "missing body file for {}: {}",
                o.key,
                o.body
            );
        }
        // every body whose config.code_file points at a sidecar must exist
        for o in &m.objects {
            let raw = std::fs::read_to_string(dir.join(&o.body)).unwrap();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
                && let Some(code_file) = v
                    .get("config")
                    .and_then(|c| c.get("code_file"))
                    .and_then(|f| f.as_str())
            {
                assert!(
                    dir.join(code_file).exists(),
                    "missing code_file sidecar for {}: {}",
                    o.key,
                    code_file
                );
            }
        }
        // every dependency resolves and there are no cycles
        m.topo_order().expect("topo order");
        // every @kind/key placeholder points at a declared key or organization/self
        let keys: std::collections::BTreeSet<&str> =
            m.objects.iter().map(|o| o.key.as_str()).collect();
        for o in &m.objects {
            let raw = std::fs::read_to_string(dir.join(&o.body)).unwrap();
            for tok in raw.split('"') {
                if let Some(rest) = tok.strip_prefix('@')
                    && let Some((kind, key)) = rest.split_once('/')
                {
                    let ok = (kind == "organization" && key == "self") || keys.contains(key);
                    assert!(ok, "{} references unknown placeholder @{}/{}", o.body, kind, key);
                }
            }
        }
    }
}
