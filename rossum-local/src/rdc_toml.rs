use anyhow::{Context, Result};
use std::path::Path;

/// Ensure `<cwd>/rdc.toml` declares `[envs.main]` with the given API
/// base and org id. Writes when missing or when the desired body
/// differs from the on-disk body; no-op when already in sync.
pub fn ensure_rdc_toml(cwd: &Path, api_base: &str, org_id: u64) -> Result<()> {
    std::fs::create_dir_all(cwd).with_context(|| format!("creating {}", cwd.display()))?;
    let path = cwd.join("rdc.toml");
    let desired = format!(
        "[envs.main]\napi_base = \"{}\"\norg_id = {}\n",
        api_base, org_id
    );
    let same = match std::fs::read_to_string(&path) {
        Ok(existing) => existing == desired,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if same {
        return Ok(());
    }
    rdc::snapshot::writer::write_atomic(&path, desired.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
