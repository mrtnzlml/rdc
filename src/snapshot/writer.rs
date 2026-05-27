use anyhow::{anyhow, Context, Result};
use std::fs;
use std::io::Write;
use std::path::Path;

/// Write `bytes` to `path` atomically: write to a sibling temp file, then rename.
/// Creates parent directories if missing.
///
/// Skips the write entirely when `path` already contains exactly these bytes,
/// so a no-op `rdc pull` doesn't bump mtimes on every file (otherwise every
/// re-pull would look like "everything changed" to mtime-aware tools).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Ok(existing) = fs::read(path)
        && existing == bytes {
            return Ok(());
        }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }
    // Sibling temp file: append ".tmp" to the full file name (not the
    // extension). Works for "foo.json" → "foo.json.tmp" and for an
    // extensionless "foo" → "foo.tmp".
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("path {} has no file name", path.display()))?;
    let mut tmp_name = file_name.to_owned();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
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

    #[test]
    fn skips_write_when_bytes_match_existing() {
        // No-op pull must not bump mtimes.
        use std::time::Duration;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("z.txt");
        write_atomic(&path, b"same").unwrap();
        let mtime_before = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        write_atomic(&path, b"same").unwrap();
        let mtime_after = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "mtime should not change when bytes match");
    }

    #[test]
    fn extensionless_path_writes_cleanly() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rdc");
        write_atomic(&path, b"binary-like").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"binary-like");
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries, vec!["rdc"]);
    }
}
