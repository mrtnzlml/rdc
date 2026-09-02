//! Live watches, keyed by `(folder, env)` — the same identity the Dart side
//! uses for its per-env state.
//!
//! Its own module rather than statics in `api::rdc` so the FFI surface stays
//! a thin translation layer: `flutter_rust_bridge_codegen` scans
//! `crate::api`, and anything public it finds there it tries to bridge.

use rdc::cli::sync::watch::CancelToken;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

/// Monotonic source of registration identities. `insert` can silently
/// displace whatever handle already sits at a key (by design — see its own
/// doc), so "the handle I registered" and "whatever now occupies this key"
/// can diverge: a caller that removed by key alone could delete a DIFFERENT
/// registration that has since taken its place. Every caller stamps its
/// handle with a fresh id from here and tears down only through
/// `remove_if`, which checks that id back against the map.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::SeqCst)
}

/// Monotonic source of prompt ids, shared by every `SinkPromptRoute` in the
/// process rather than counted per `WatchHandle`. A displaced watch's
/// generation can still be unwinding a blocked `ask()` when the cycle that
/// displaced it issues its own first prompt; if each counted from 1 the two
/// would collide on id `1`, and `SyncPhase::PromptResolved` (which carries
/// only a bare id, no generation tag) from the stale generation could clear
/// the live generation's prompt out from under the user. A single
/// process-global counter makes that collision impossible.
static NEXT_PROMPT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_prompt_id() -> u64 {
    NEXT_PROMPT_ID.fetch_add(1, Ordering::SeqCst)
}

#[derive(Clone)]
pub struct WatchHandle {
    /// This registration's identity, from `next_id()`. Compared by
    /// `remove_if` so a stale caller can't delete a newer registration that
    /// has displaced it at the same `(folder, env)` key.
    pub id: u64,
    pub cancel: CancelToken,
    /// Answers from the UI, delivered to whichever prompt is blocked. Each
    /// answer is tagged with the id of the prompt the UI believes it is
    /// answering (`answer_prompt`'s `prompt_id`) — `SinkPromptRoute::ask`
    /// checks that tag against the id it minted for the question it is
    /// CURRENTLY asking and ignores anything else, so a reply to a prompt
    /// that has already moved on can never be misapplied to the next one.
    pub answers: Sender<(u64, String)>,
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

/// Remove the entry at `(folder, env)` only if it is still the registration
/// identified by `expected_id`. A cycle that registered, got displaced by a
/// later registration (see `insert`), and only THEN reached its own
/// teardown must not delete what displaced it — that would orphan the new
/// registration: `answer_prompt` would find nothing at the key, and any
/// prompt the new cycle is blocked on would have no way for `stop_watch` (or
/// a later `answer_prompt`) to ever reach it again.
pub fn remove_if(folder: &str, env: &str, expected_id: u64) -> Option<WatchHandle> {
    let mut reg = registry().lock().unwrap();
    let key = (folder.to_string(), env.to_string());
    match reg.get(&key) {
        Some(h) if h.id == expected_id => reg.remove(&key),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> (WatchHandle, u64) {
        let (tx, _rx) = std::sync::mpsc::channel();
        let id = next_id();
        (
            WatchHandle {
                id,
                cancel: CancelToken::new(),
                answers: tx,
            },
            id,
        )
    }

    // Every test below uses its own `(folder, env)` key. The registry is a
    // process-global singleton and `cargo test` runs tests in parallel
    // threads by default, so two tests sharing a key can race on it —
    // exactly the class of bug `remove_if` exists to guard against, just
    // between test threads instead of watch/sync cycles. Unique keys avoid
    // relying on scheduling luck rather than papering over it.

    #[test]
    fn insert_returns_the_displaced_handle() {
        let (first, _) = handle();
        assert!(insert("/tmp/insert-displace-test", "dev", first.clone()).is_none());
        let (second, _) = handle();
        let displaced =
            insert("/tmp/insert-displace-test", "dev", second).expect("should displace");
        displaced.cancel.cancel();
        assert!(displaced.cancel.is_cancelled());
        remove_if("/tmp/insert-displace-test", "dev", displaced.id);
    }

    #[test]
    fn entries_are_scoped_per_folder_and_env() {
        let (h, id) = handle();
        insert("/tmp/scope-test", "dev", h);
        assert!(get("/tmp/scope-test", "dev").is_some());
        assert!(get("/tmp/scope-test", "prod").is_none());
        assert!(get("/tmp/scope-test-other", "dev").is_none());
        remove_if("/tmp/scope-test", "dev", id);
    }

    #[test]
    fn remove_if_cannot_delete_a_registration_that_displaced_it() {
        // Simulates the observed clobber: a watch registers (H1), then a
        // one-shot sync on the same env registers on top of it (H2,
        // displacing H1 — that part is correct, existing behaviour). H1's
        // owner then tears down and calls what it thinks is "remove my
        // entry" — that must not be able to delete H2's entry instead.
        let key_folder = "/tmp/gen-id-test";
        let key_env = "dev";
        let (h1, h1_id) = handle();
        assert!(insert(key_folder, key_env, h1).is_none());
        let (h2, h2_id) = handle();
        let displaced = insert(key_folder, key_env, h2).expect("should displace h1");
        assert_eq!(displaced.id, h1_id);

        // The stale identity (H1's) must not be able to remove H2.
        assert!(
            remove_if(key_folder, key_env, h1_id).is_none(),
            "a stale id must not delete a newer registration"
        );
        assert!(
            get(key_folder, key_env).is_some(),
            "the newer registration (H2) must survive the stale caller's teardown"
        );

        // The current identity (H2's) does remove it.
        assert!(remove_if(key_folder, key_env, h2_id).is_some());
        assert!(get(key_folder, key_env).is_none());
    }
}
