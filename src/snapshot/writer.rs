use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::Path;

/// Write `bytes` to `path` atomically: write to a sibling temp file, then rename.
/// Creates parent directories if missing.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("creating temp file {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("writing temp file {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("syncing temp file {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn writes_bytes_to_new_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a/b/c.txt");
        write_atomic(&path, b"hello").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.txt");
        write_atomic(&path, b"v1").unwrap();
        write_atomic(&path, b"v2").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"v2");
    }

    #[test]
    fn temp_file_does_not_persist_on_success() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("y.txt");
        write_atomic(&path, b"data").unwrap();
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries, vec!["y.txt"]);
    }
}
