//! Where an env's sync state lives between runs.
//!
//! A sync reads and writes its state (`.rdc/state/<env>.lock.json`, the base
//! cache, conflict shadows) through the vfs. A [`StateStore`] brings that
//! state into the vfs before the run reads it ([`StateStore::hydrate`]) and
//! makes it durable after every lockfile persist ([`StateStore::checkpoint`]).
//! On disk both are no-ops ([`FsStateStore`]); a host whose vfs is in memory
//! supplies a store that keeps the state somewhere that outlives the page.

use crate::log::{Action, Log};
use crate::paths::Paths;
use crate::rt::BoxFuture;
use crate::state::journal::{Journal, PersistFn};
use crate::state::Lockfile;
use anyhow::Result;
use std::future::Future;
use std::sync::Arc;

/// An env's sync state, kept outside the vfs. `Send + Sync` like
/// [`crate::cli::stdin_coord::PromptRoute`]; its futures are `Send` natively
/// and local on wasm32, as [`crate::rt::BoxFuture`] is.
pub trait StateStore: Send + Sync {
    /// Bring this env's state into the vfs before the run reads it.
    fn hydrate<'a>(&'a self, paths: &'a Paths) -> BoxFuture<'a, Result<()>>;
    /// Make the vfs state durable. Called after every journal persist and commit.
    fn checkpoint<'a>(&'a self, paths: &'a Paths) -> BoxFuture<'a, Result<()>>;
    /// Problems the checkpoints since the last call met that left the state
    /// durable (a failed clean-up). The sync shows each as a warning.
    fn take_warnings(&self) -> Vec<String> {
        Vec::new()
    }
    /// The token the cycle about to start talks to Rossum with; called before
    /// its hydrate when the caller supplied one. A watch refreshes its token
    /// after a 401, so a store that calls Rossum itself must not keep the
    /// token it was built with.
    fn use_token(&self, _token: &str) {}
}

/// The state is the files on disk, written atomically already: both methods
/// do nothing. The CLI's store, and the default when none is scoped.
pub struct FsStateStore;

impl StateStore for FsStateStore {
    fn hydrate<'a>(&'a self, _paths: &'a Paths) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn checkpoint<'a>(&'a self, _paths: &'a Paths) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

tokio::task_local! {
    static STATE: Arc<dyn StateStore>;
}

/// Run `f` with `state` as the [`StateStore`] every sync inside it uses.
/// Task-local for the same reason as `vfs::scope`: the continuation may
/// resume on another worker thread, and the store must travel with the task.
pub async fn scope<F: Future>(state: Arc<dyn StateStore>, f: F) -> F::Output {
    STATE.scope(state, f).await
}

/// The store this task is scoped to, or an [`FsStateStore`] when none is.
pub fn current() -> Arc<dyn StateStore> {
    scoped().unwrap_or_else(|| Arc::new(FsStateStore))
}

/// The store this task is scoped to, if any. [`crate::rt::spawn`] carries it
/// into the spawned task.
pub fn scoped() -> Option<Arc<dyn StateStore>> {
    STATE.try_with(Arc::clone).ok()
}

/// The sync's journal over `committed`: each persist saves the lockfile to
/// `paths.lockfile()`, then checkpoints `store` and shows its warnings on `log`.
pub fn journal_for(paths: &Paths, committed: Lockfile, store: Arc<dyn StateStore>, log: Arc<Log>) -> Journal {
    let paths = paths.clone();
    let persist: PersistFn = Arc::new(move |lf: &Lockfile| {
        let saved = lf.save(&paths.lockfile());
        let paths = paths.clone();
        let store = Arc::clone(&store);
        let log = Arc::clone(&log);
        Box::pin(async move {
            saved?;
            store.checkpoint(&paths).await?;
            for w in store.take_warnings() {
                log.event(Action::Warn, &w);
            }
            Ok(())
        })
    });
    Journal::new(committed, persist, crate::state::journal::system_now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{MemStore, Store};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts checkpoints; each one asserts the lockfile was saved first.
    struct Counting(AtomicUsize);
    impl StateStore for Counting {
        fn hydrate<'a>(&'a self, _: &'a Paths) -> BoxFuture<'a, Result<()>> {
            self.0.fetch_add(100, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn checkpoint<'a>(&'a self, paths: &'a Paths) -> BoxFuture<'a, Result<()>> {
            assert!(crate::vfs::read(paths.lockfile()).is_ok(), "checkpoint before save");
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn current_defaults_to_fs_and_follows_scope() {
        let paths = Paths::for_env(std::path::Path::new("/p"), "dev");
        let counting = Arc::new(Counting(AtomicUsize::new(0)));
        scope(counting.clone(), async { current().hydrate(&paths).await }).await.unwrap();
        current().hydrate(&paths).await.unwrap();
        assert_eq!(counting.0.load(Ordering::SeqCst), 100, "only the scoped call reaches it");
    }

    /// A store whose every checkpoint leaves one warning.
    struct Warning;
    impl StateStore for Warning {
        fn hydrate<'a>(&'a self, _: &'a Paths) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn checkpoint<'a>(&'a self, _: &'a Paths) -> BoxFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn take_warnings(&self) -> Vec<String> {
            vec!["clean-up failed".to_string()]
        }
    }

    #[derive(Clone, Default)]
    struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn journal_for_shows_checkpoint_warnings() {
        let root = std::path::PathBuf::from("/p");
        let mem: Arc<dyn Store> = Arc::new(MemStore::new(root.clone()));
        let paths = Paths::for_env(&root, "dev");
        let buf = Buf::default();
        let log = Log::for_sink(crate::cli::resolve::ColorMode::Plain, Box::new(buf.clone()));
        crate::vfs::scope(mem, async {
            let j = journal_for(&paths, Lockfile::default(), Arc::new(Warning), log);
            j.commit(&Lockfile::default()).await.unwrap();
        })
        .await;
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains("clean-up failed"), "{out}");
    }

    #[tokio::test]
    async fn journal_for_saves_then_checkpoints() {
        let root = std::path::PathBuf::from("/p");
        let mem: Arc<dyn Store> = Arc::new(MemStore::new(root.clone()));
        let paths = Paths::for_env(&root, "dev");
        let counting = Arc::new(Counting(AtomicUsize::new(0)));
        crate::vfs::scope(mem, async {
            let j = journal_for(&paths, Lockfile::default(), counting.clone(), Log::new(crate::cli::resolve::ColorMode::Plain));
            j.commit(&Lockfile::default()).await.unwrap();
            let saved = Lockfile::load(&paths.lockfile()).unwrap();
            assert_eq!(saved.pending, None);
        })
        .await;
        assert_eq!(counting.0.load(Ordering::SeqCst), 1);
    }
}
