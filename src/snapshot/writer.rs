use anyhow::{anyhow, Context, Result};
use crate::vfs as fs;
use std::path::Path;

/// Write `bytes` to `path` atomically: write to a sibling temp file, then rename.
/// Creates parent directories if missing.
///
/// Skips the write entirely when `path` already contains exactly these bytes,
/// so a no-op pull doesn't bump mtimes on every file (otherwise every
/// re-pull would look like "everything changed" to mtime-aware tools).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_with_mode(path, bytes, None)
}

/// [`write_atomic`] for secrets: on Unix the temp file is created with mode
/// `0600`, so the bytes are never readable by others, not even before the
/// rename (which keeps the mode).
pub fn write_atomic_private(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic_with_mode(path, bytes, Some(0o600))
}

fn write_atomic_with_mode(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    if let Ok(existing) = fs::read(path)
        && existing == bytes {
            return Ok(());
        }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }
    // No crash to survive and no other process to hide a half-written file
    // from, and the temp file and rename would each reach the host as a change.
    if !fs::is_native() {
        return fs::write(path, bytes).with_context(|| format!("writing {}", path.display()));
    }
    // Sibling temp file with a random suffix, opened with `create_new`: a
    // fixed name could be pre-planted in the repo as a symlink, and
    // `create_new` refuses any existing entry, a symlink included.
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("path {} has no file name", path.display()))?;
    let mut rand = [0u8; 8];
    getrandom::fill(&mut rand).map_err(|e| anyhow!("reading OS randomness: {e}"))?;
    let mut tmp_name = file_name.to_owned();
    tmp_name.push(format!(".{}.tmp", rand.iter().map(|b| format!("{b:02x}")).collect::<String>()));
    let tmp = path.with_file_name(tmp_name);
    let result = (|| -> Result<()> {
        // `write_new` creates and fills in one call; the file's presence after
        // a failure says which half failed.
        fs::write_new(&tmp, bytes, mode).with_context(|| {
            let verb = if fs::exists(&tmp) { "writing" } else { "creating" };
            format!("{verb} temp file {}", tmp.display())
        })?;
        fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))
    })();
    if result.is_err() {
        // The random name is never reused, so a failed write cleans up after itself.
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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

    /// A repo can carry a committed `<file>.tmp` symlink; writing the file
    /// must not write through it into the link's target.
    #[cfg(unix)]
    #[test]
    fn does_not_write_through_a_planted_temp_symlink() {
        let dir = TempDir::new().unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, b"orig").unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join("f.json.tmp")).unwrap();
        write_atomic(&dir.path().join("f.json"), b"new").unwrap();
        assert_eq!(fs::read(&outside).unwrap(), b"orig");
        assert_eq!(fs::read(dir.path().join("f.json")).unwrap(), b"new");
    }

    /// A secret must never sit in a file others can read, not even the temp
    /// file before the rename.
    #[cfg(unix)]
    #[test]
    fn private_write_creates_an_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.json");
        write_atomic_private(&path, b"secret").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read(&path).unwrap(), b"secret");
    }

    /// A non-native store has no crash to survive, and the temp file and the
    /// rename would each be a change event for the host.
    #[test]
    fn non_native_store_gets_one_event_per_write() {
        use crate::vfs::{MemStore, Store};
        use std::sync::{Arc, Mutex};
        let mem = Arc::new(MemStore::new("/rdc-mem".into()));
        let log = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
        let sink = log.clone();
        mem.set_on_change(Box::new(move |k, v| sink.lock().unwrap().push((k.to_string(), v.is_some()))));
        let store: Arc<dyn Store> = mem.clone();
        crate::vfs::scope_sync(store, || {
            write_atomic(Path::new("/rdc-mem/a/b.txt"), b"hi").unwrap();
        });
        assert_eq!(*log.lock().unwrap(), [("a/b.txt".to_string(), true)]);
        assert_eq!(mem.files().len(), 1);
    }

    /// Failing to create the temp file and failing to fill it are different
    /// problems, and the message says which.
    #[cfg(unix)]
    #[test]
    fn creation_failure_says_creating() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let probe = fs::write(dir.path().join("probe"), b"");
        let r = write_atomic(&dir.path().join("f"), b"x");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        if probe.is_ok() {
            return; // running as root: permissions are not enforced
        }
        let msg = format!("{:#}", r.unwrap_err());
        assert!(msg.contains("creating temp file"), "{msg}");
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
