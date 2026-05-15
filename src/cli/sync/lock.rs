//! Advisory cross-process lock for serializing writers on one env.
//!
//! Sync (one-shot and watch) and deploy (writes to target env) acquire
//! this lock for the duration of their execute phase. The lock file
//! lives at `Paths::env_lock()` — sibling of the JSON lockfile.
//!
//! Crash safety: the OS releases the lock on process exit. The empty
//! lock file is left behind; subsequent runs reuse it.

use anyhow::{Context, Result};
use fs4::{FileExt, TryLockError};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::Duration;

// Disambiguate from the inherent `try_lock` / `unlock` methods on
// `std::fs::File` (stable since Rust 1.89). We call fs4's trait
// methods explicitly via fully-qualified syntax below so the trait
// import is genuinely used and the behaviour stays sourced from fs4.

#[derive(Debug)]
pub struct EnvLock {
    file: File,
}

impl EnvLock {
    /// Acquire an exclusive lock, waiting up to `timeout`. Polls every
    /// 200 ms while blocked. Creates the lock file (and the parent
    /// directory) if needed.
    pub fn acquire(lock_path: &Path, timeout: Duration) -> Result<Self> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating lock parent dir {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .with_context(|| format!("opening lock file {}", lock_path.display()))?;

        let deadline = std::time::Instant::now() + timeout;
        loop {
            match <File as FileExt>::try_lock(&file) {
                Ok(()) => return Ok(EnvLock { file }),
                Err(TryLockError::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!(
                            "timed out after {:?} waiting for env lock at {}",
                            timeout,
                            lock_path.display()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(TryLockError::Error(e)) => {
                    return Err(e).with_context(|| {
                        format!("acquiring exclusive lock on {}", lock_path.display())
                    });
                }
            }
        }
    }
}

impl Drop for EnvLock {
    fn drop(&mut self) {
        let _ = <File as FileExt>::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn acquire_succeeds_on_unheld_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.lock");
        let _lock = EnvLock::acquire(&path, Duration::from_secs(1)).unwrap();
        assert!(path.exists(), "lock file should be created");
    }

    #[test]
    fn second_acquire_times_out_when_first_held() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.lock");
        let _first = EnvLock::acquire(&path, Duration::from_secs(1)).unwrap();
        let err = EnvLock::acquire(&path, Duration::from_millis(300)).unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
    }

    #[test]
    fn second_acquire_succeeds_after_first_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.lock");
        let barrier = Arc::new(Barrier::new(2));
        let path2 = path.clone();
        let b2 = barrier.clone();
        let handle = thread::spawn(move || {
            let lock = EnvLock::acquire(&path2, Duration::from_secs(1)).unwrap();
            b2.wait();
            thread::sleep(Duration::from_millis(200));
            drop(lock);
        });
        barrier.wait();
        let _second = EnvLock::acquire(&path, Duration::from_secs(2))
            .expect("should acquire after first thread drops");
        handle.join().unwrap();
    }
}
