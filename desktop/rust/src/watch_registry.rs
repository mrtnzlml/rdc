//! Live watches, keyed by `(folder, env)` — the same identity the Dart side
//! uses for its per-env state.
//!
//! Its own module rather than statics in `api::rdc` so the FFI surface stays
//! a thin translation layer: `flutter_rust_bridge_codegen` scans
//! `crate::api`, and anything public it finds there it tries to bridge.

use rdc::cli::sync::watch::CancelToken;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone)]
pub struct WatchHandle {
    pub cancel: CancelToken,
    /// Answers from the UI, delivered to whichever prompt is blocked.
    pub answers: Sender<String>,
    pub next_prompt_id: Arc<AtomicU64>,
}

type Map = HashMap<(String, String), WatchHandle>;

fn registry() -> &'static Mutex<Map> {
    static R: OnceLock<Mutex<Map>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Insert, returning any handle that was already there (which the caller
/// must cancel — two watches on one env would fight over the env lock).
pub fn insert(folder: &str, env: &str, h: WatchHandle) -> Option<WatchHandle> {
    registry()
        .lock()
        .unwrap()
        .insert((folder.to_string(), env.to_string()), h)
}

pub fn get(folder: &str, env: &str) -> Option<WatchHandle> {
    registry()
        .lock()
        .unwrap()
        .get(&(folder.to_string(), env.to_string()))
        .cloned()
}

pub fn remove(folder: &str, env: &str) -> Option<WatchHandle> {
    registry()
        .lock()
        .unwrap()
        .remove(&(folder.to_string(), env.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> WatchHandle {
        let (tx, _rx) = std::sync::mpsc::channel();
        WatchHandle {
            cancel: CancelToken::new(),
            answers: tx,
            next_prompt_id: Arc::new(AtomicU64::new(1)),
        }
    }

    #[test]
    fn insert_returns_the_displaced_handle() {
        let first = handle();
        assert!(insert("/tmp/acme", "dev", first.clone()).is_none());
        let displaced = insert("/tmp/acme", "dev", handle()).expect("should displace");
        displaced.cancel.cancel();
        assert!(displaced.cancel.is_cancelled());
        remove("/tmp/acme", "dev");
    }

    #[test]
    fn entries_are_scoped_per_folder_and_env() {
        insert("/tmp/acme", "dev", handle());
        assert!(get("/tmp/acme", "dev").is_some());
        assert!(get("/tmp/acme", "prod").is_none());
        assert!(get("/tmp/beta", "dev").is_none());
        remove("/tmp/acme", "dev");
    }
}
