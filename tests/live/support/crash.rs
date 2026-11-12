//! Interrupting a sync from the client side: a state store that never
//! returns from a chosen checkpoint, and the in-process run it interrupts.
//! Shared by the fake crash matrix (`fake::crash`) and the live crash
//! scenarios (`scenarios::crash_recovery`), so nothing here may depend on
//! the fake org.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use rdc::cli::sync::embed::EmbedSyncOptions;
use rdc::paths::Paths;
use rdc::rt::BoxFuture;
use rdc::state::store::StateStore;
use rdc::state::Lockfile;
use std::future::Future;
use std::path::Path;
use tokio::sync::Notify;

/// A [`StateStore`] that never returns from the first checkpoint whose
/// lockfile `when` accepts, and signals `fired` there. With `undo` it first
/// puts back the lockfile of the checkpoint before — the persist it was
/// making durable never reached the disk.
pub struct CrashStore {
    when: Box<dyn Fn(&Lockfile) -> bool + Send + Sync>,
    undo: bool,
    armed: AtomicBool,
    /// The lockfile bytes as of the last checkpoint that returned (or the
    /// hydrate), `None` while there is no lockfile.
    durable: Mutex<Option<Vec<u8>>>,
    /// Every lockfile a checkpoint was asked to make durable, in order.
    seen: Mutex<Vec<Vec<u8>>>,
    /// The store the run really keeps its state in, if any: hydrates and
    /// every checkpoint but the crashing one go to it.
    inner: Option<Arc<dyn StateStore>>,
    pub fired: Arc<Notify>,
}

impl CrashStore {
    pub fn new(when: impl Fn(&Lockfile) -> bool + Send + Sync + 'static, undo: bool) -> Arc<CrashStore> {
        Arc::new(CrashStore {
            when: Box::new(when),
            undo,
            armed: AtomicBool::new(true),
            durable: Mutex::default(),
            seen: Mutex::default(),
            inner: None,
            fired: Arc::new(Notify::new()),
        })
    }

    /// [`CrashStore::new`] over `inner`: the state lives in `inner`, which
    /// never sees the crashing checkpoint, so it keeps the one before.
    pub fn wrapping(
        inner: Arc<dyn StateStore>,
        when: impl Fn(&Lockfile) -> bool + Send + Sync + 'static,
        undo: bool,
    ) -> Arc<CrashStore> {
        Arc::new(CrashStore {
            when: Box::new(when),
            undo,
            armed: AtomicBool::new(true),
            durable: Mutex::default(),
            seen: Mutex::default(),
            inner: Some(inner),
            fired: Arc::new(Notify::new()),
        })
    }

    /// The lockfiles its checkpoints saw.
    pub fn seen(&self) -> Vec<Vec<u8>> {
        self.seen.lock().unwrap().clone()
    }
}

impl StateStore for CrashStore {
    fn hydrate<'a>(&'a self, paths: &'a Paths) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            if let Some(inner) = &self.inner {
                inner.hydrate(paths).await?;
            }
            *self.durable.lock().unwrap() = rdc::vfs::read(paths.lockfile()).ok();
            Ok(())
        })
    }

    fn take_warnings(&self) -> Vec<String> {
        self.inner.as_ref().map(|i| i.take_warnings()).unwrap_or_default()
    }

    fn use_token(&self, token: &str) {
        if let Some(inner) = &self.inner {
            inner.use_token(token);
        }
    }

    fn checkpoint<'a>(&'a self, paths: &'a Paths) -> BoxFuture<'a, anyhow::Result<()>> {
        let bytes = rdc::vfs::read(paths.lockfile()).ok();
        self.seen.lock().unwrap().extend(bytes.clone());
        let lf = Lockfile::load(&paths.lockfile()).expect("the lockfile just saved parses");
        if !(self.armed.load(Ordering::SeqCst) && (self.when)(&lf)) {
            *self.durable.lock().unwrap() = bytes;
            return match &self.inner {
                Some(inner) => inner.checkpoint(paths),
                None => Box::pin(async { Ok(()) }),
            };
        }
        self.armed.store(false, Ordering::SeqCst);
        if self.undo {
            match self.durable.lock().unwrap().as_ref() {
                Some(before) => rdc::vfs::write(paths.lockfile(), before).expect("restoring the lockfile"),
                None => rdc::vfs::remove_file(paths.lockfile()).expect("removing the lockfile"),
            }
        }
        self.fired.notify_one();
        Box::pin(std::future::pending())
    }
}

/// A checkpoint whose journal has an op for `kind`/`name` with no id: the
/// state right after that create's begin.
pub fn begun(kind: &'static str, name: &str) -> impl Fn(&Lockfile) -> bool + Send + Sync + 'static {
    let name = name_hash(name);
    move |lf| ops(lf).any(|op| op.kind == kind && op.fields.get("name") == Some(&name) && op.id.is_none())
}

/// A checkpoint whose journal has an op for `kind`/`name` with its id: the
/// state right after that create's finish.
pub fn finished(kind: &'static str, name: &str) -> impl Fn(&Lockfile) -> bool + Send + Sync + 'static {
    let name = name_hash(name);
    move |lf| ops(lf).any(|op| op.kind == kind && op.fields.get("name") == Some(&name) && op.id.is_some())
}

fn ops(lf: &Lockfile) -> impl Iterator<Item = &rdc::state::journal::CreateIntent> {
    lf.pending.iter().flat_map(|p| p.ops.iter())
}

/// The journal's `name` field for a body named `name`.
fn name_hash(name: &str) -> String {
    rdc::state::journal::match_fields(&serde_json::json!({ "name": name })).remove("name").unwrap()
}

/// The options of a `rdc sync <env> --yes`, plus `--allow-deletes` when asked.
pub fn yes(allow_deletes: bool) -> EmbedSyncOptions {
    EmbedSyncOptions { interactive: false, allow_deletes, ..Default::default() }
}

/// One `rdc sync test` of the project at `root`, in this process, with
/// `state` as its state store.
pub fn sync_in_process<'a>(
    root: &'a Path,
    token: &'a str,
    opts: EmbedSyncOptions,
    state: Arc<dyn StateStore>,
) -> impl Future<Output = anyhow::Result<rdc::cli::sync::CycleOutcome>> + 'a {
    rdc::state::store::scope(
        state,
        Box::pin(rdc::cli::sync::embed::sync_logged(root, "test", token, opts, Box::new(std::io::sink()))),
    )
}

/// Drive `run` until `fired`, then drop it: the process died there. Panics if
/// the run ends first — the crash point was never reached.
pub async fn until_crash<T>(run: impl Future<Output = anyhow::Result<T>>, fired: &Notify) {
    tokio::select! {
        biased;
        _ = fired.notified() => {}
        out = run => panic!("the run ended before the crash point: {:?}", out.err()),
    }
}
