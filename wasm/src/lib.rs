//! JavaScript bindings for rdc on an in-memory project tree (the browser
//! build). The host hands over the project as a map of relative path to
//! bytes, rdc runs on it, and the host reads the changed files back (or
//! listens for them with `onChange`). Where the tree is stored is the host's
//! business.
//!
//! The logic lives in [`ProjectCore`], plain Rust that runs and is tested
//! natively. `bindings` (wasm32 only) is the thin `#[wasm_bindgen]` layer
//! that converts JS values and callbacks into it.

use anyhow::{Result, anyhow};
use rdc::cli::sync::embed;
use rdc::state::store::{FsStateStore, StateStore};
use rdc::vfs::{MemStore, Store};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(target_arch = "wasm32")]
mod bindings;

/// The virtual root the tree lives under. Internal: the host only ever sees
/// keys relative to it.
const ROOT: &str = "/project";

// ---------------------------------------------------------------- DTOs
//
// Plain objects on the JS side. They mirror the desktop bridge's DTOs
// (`desktop/rust/src/api/rdc.rs`) field for field, in camelCase.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthKind {
    Token,
    Password,
}

impl From<AuthKind> for embed::AuthKind {
    fn from(k: AuthKind) -> Self {
        match k {
            AuthKind::Token => embed::AuthKind::Token,
            AuthKind::Password => embed::AuthKind::Password,
        }
    }
}

impl From<embed::AuthKind> for AuthKind {
    fn from(k: embed::AuthKind) -> Self {
        match k {
            embed::AuthKind::Token => AuthKind::Token,
            embed::AuthKind::Password => AuthKind::Password,
        }
    }
}

/// The desktop's `AddEnvInput`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvInput {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

impl From<&EnvInput> for embed::EnvInput {
    fn from(i: &EnvInput) -> Self {
        Self {
            name: i.name.clone(),
            api_base: i.api_base.clone(),
            org_id: i.org_id,
            auth_kind: i.auth_kind.into(),
            token: i.token.clone(),
            username: i.username.clone(),
            password: i.password.clone(),
        }
    }
}

/// The desktop's `EditConnectionInput`, plus `env`: the desktop passes the
/// env as its own argument, `editProject` takes one object.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditProjectInput {
    pub env: String,
    /// The project name. There is no folder to rename; it only changes the
    /// summary's `id`/`name`.
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvSummary {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    /// Always `null`: an in-memory tree keeps no timestamps.
    pub last_sync_unix: Option<i64>,
    pub file_count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSummary {
    /// The slug of the name given to `create` or `editProject`; empty for a
    /// project opened from files until one is given.
    pub id: String,
    pub name: String,
    /// Always empty: where the tree is stored is the host's business.
    pub folder: String,
    pub envs: Vec<EnvSummary>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameEnvResult {
    pub project: ProjectSummary,
    pub follow_ups: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictStrategy {
    UseRemote,
    KeepLocal,
    Skip,
}

/// `embed::EmbedSyncOptions`, with the same defaults. Every field may be
/// left out.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncOptions {
    pub interactive: bool,
    pub allow_deletes: bool,
    pub no_push: bool,
    pub no_pull: bool,
    pub dry_run: bool,
    pub conflict: Option<ConflictStrategy>,
}

impl Default for SyncOptions {
    fn default() -> Self {
        let d = embed::EmbedSyncOptions::default();
        Self {
            interactive: d.interactive,
            allow_deletes: d.allow_deletes,
            no_push: d.no_push,
            no_pull: d.no_pull,
            dry_run: d.dry_run,
            conflict: None,
        }
    }
}

impl From<SyncOptions> for embed::EmbedSyncOptions {
    fn from(o: SyncOptions) -> Self {
        use rdc::cli::resolve::ConflictStrategy as C;
        Self {
            interactive: o.interactive,
            allow_deletes: o.allow_deletes,
            no_push: o.no_push,
            no_pull: o.no_pull,
            dry_run: o.dry_run,
            conflict: o.conflict.map(|c| match c {
                ConflictStrategy::UseRemote => C::UseRemote,
                ConflictStrategy::KeepLocal => C::KeepLocal,
                ConflictStrategy::Skip => C::Skip,
            }),
        }
    }
}

/// One cycle's result: the core's counts, plus the desktop `Done` phase's
/// file count.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CycleOutcome {
    pub items_pushed: usize,
    pub items_pulled: usize,
    pub conflicts: usize,
    pub remote_deletes_resolved: usize,
    pub file_count: u64,
}

/// The desktop's `PromptKindDto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PromptKind {
    Conflict,
    RemoteDelete,
    BulkConfirm,
    DeleteGate,
    DeleteDrift,
    MdhIndexDrop,
    MdhRowDelete,
    /// A prompt whose site never announced. Should be unreachable; render
    /// it as an explicit "unrecognised prompt", not as a normal dialog.
    Unknown,
}

/// No catch-all arm on purpose: a new core kind must fail to compile here
/// rather than fall through to a wrong label on a flow that can delete
/// objects from a live organization.
fn kind_of(k: embed::PromptKind) -> PromptKind {
    use embed::PromptKind as K;
    match k {
        K::Conflict => PromptKind::Conflict,
        K::RemoteDelete => PromptKind::RemoteDelete,
        K::BulkConfirm => PromptKind::BulkConfirm,
        K::DeleteGate => PromptKind::DeleteGate,
        K::DeleteDrift => PromptKind::DeleteDrift,
        K::MdhIndexDrop => PromptKind::MdhIndexDrop,
        K::MdhRowDelete => PromptKind::MdhRowDelete,
        K::Unknown => PromptKind::Unknown,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptChoice {
    /// Always exactly one character.
    pub key: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Prompt {
    pub kind: PromptKind,
    pub question: String,
    pub choices: Vec<PromptChoice>,
}

// ---------------------------------------------------------------- prompts and log

/// What the host is shown for `prompt`, and the keys it may answer with.
///
/// `[e]` (opens `$EDITOR`) and `[h]` (walks hunks) mean nothing outside a
/// terminal, so they are dropped, and the question is rebuilt from what is
/// left — the terminal's own line names choices this surface does not offer.
fn offer(prompt: &embed::Prompt) -> (Prompt, HashSet<char>) {
    let offered: Vec<embed::PromptKey> =
        prompt.keys.iter().filter(|k| !matches!(k.key, 'e' | 'h')).cloned().collect();
    let allowed = offered.iter().map(|k| k.key).collect();
    let dto = Prompt {
        kind: kind_of(prompt.kind),
        question: embed::menu_one_line(&offered),
        choices: offered
            .iter()
            .map(|k| PromptChoice { key: k.key.to_string(), label: k.label.clone() })
            .collect(),
    };
    (dto, allowed)
}

/// The host's answer, if it is one of the offered keys. Anything else —
/// including an `e`/`h` that would spawn `$EDITOR` — reads as no answer
/// (end of input: skip / N). An empty `allowed` is a free-text prompt
/// (`PromptKind::Unknown`) and takes whatever arrives.
fn accept(answer: Option<String>, allowed: &HashSet<char>) -> Option<String> {
    let a = answer?;
    let first = a.trim().chars().next();
    (allowed.is_empty() || first.is_some_and(|c| allowed.contains(&c))).then_some(a)
}

/// Bytes the log has written that do not yet end in a newline. Shared with
/// the prompt route: a prompt's question is written with no newline (right
/// for a terminal), and left here it would be glued onto the next log line
/// once the prompt is answered. The route drops it before asking; the
/// `Prompt` carries the question itself.
#[derive(Clone, Default)]
pub struct LineBuffer(Arc<Mutex<Vec<u8>>>);

impl LineBuffer {
    fn discard(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// The log sink: splits rdc's rendered output into lines and hands each
/// non-empty one to `emit`, without its line ending.
pub struct LineSink<F> {
    buf: LineBuffer,
    emit: F,
}

impl<F: Fn(&str)> Write for LineSink<F> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut lines = Vec::new();
        {
            let mut buf = self.buf.0.lock().unwrap();
            buf.extend_from_slice(data);
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let text = String::from_utf8_lossy(&line).trim_end_matches(['\n', '\r']).to_string();
                if !text.is_empty() {
                    lines.push(text);
                }
            }
        }
        // Outside the lock: `emit` is host code.
        for l in &lines {
            (self.emit)(l);
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A tree-change callback: the key, and the new bytes (`None` for a removal).
pub type OnChange = dyn Fn(&str, Option<&[u8]>) + Send + Sync;

/// The host's answer to one prompt, as a future.
pub type AskFn = dyn Fn(Prompt) -> rdc::rt::BoxFuture<'static, Option<String>> + Send + Sync;

/// A [`embed::PromptRoute`] over a host callback.
pub struct Route {
    ask: Box<AskFn>,
    log_buf: LineBuffer,
    /// A watch's stop: a prompt pending when it fires reads as no answer.
    cancel: Option<rdc::cli::sync::watch::CancelToken>,
}

impl embed::PromptRoute for Route {
    fn ask<'a>(&'a self, prompt: &'a embed::Prompt) -> rdc::rt::BoxFuture<'a, Option<String>> {
        let (dto, allowed) = offer(prompt);
        self.log_buf.discard();
        let answer = (self.ask)(dto);
        let cancel = self.cancel.clone();
        Box::pin(async move {
            let answer = match cancel {
                Some(c) => {
                    let stopped = Box::pin(async move { c.cancelled().await });
                    match futures::future::select(answer, stopped).await {
                        futures::future::Either::Left((a, _)) => a,
                        futures::future::Either::Right(_) => None,
                    }
                }
                None => answer.await,
            };
            accept(answer, &allowed)
        })
    }
}

/// A log sink and a prompt route that share one [`LineBuffer`]. `cancel`
/// is a watch's stop.
pub fn log_and_route(
    emit: impl Fn(&str) + Send + 'static,
    ask: Box<AskFn>,
    cancel: Option<rdc::cli::sync::watch::CancelToken>,
) -> (Box<dyn Write + Send>, Arc<dyn embed::PromptRoute>) {
    let buf = LineBuffer::default();
    let sink = LineSink { buf: buf.clone(), emit };
    (Box::new(sink), Arc::new(Route { ask, log_buf: buf, cancel }))
}

// ---------------------------------------------------------------- state and locks

/// `useDataStorageState`'s chunk size when `chunkBytes` is left out.
pub const DEFAULT_CHUNK_BYTES: usize = 512 * 1024;

fn default_chunk_bytes() -> usize {
    DEFAULT_CHUNK_BYTES
}

/// `useDataStorageState`'s options. There is no collection to choose: the
/// state always lives in [`rdc::state::data_storage_store::STATE_COLLECTION`],
/// which a sync never mirrors as an MDH dataset.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataStorageStateOptions {
    /// Names the project's state; the host knows its org and project.
    pub key: String,
    #[serde(default = "default_chunk_bytes")]
    pub chunk_bytes: usize,
}

/// A held env lock; dropping it releases the lock.
pub type HeldLock = Box<dyn Send>;

/// The browser's `EnvLock`: exclusive named locks shared by every tab of the
/// origin, held for a whole sync or watch. On wasm32 it is the Web Locks API
/// (`bindings::WebLocks`); natively, [`MemLocks`].
pub trait EnvLocks: Send + Sync {
    /// Take `name` if it is free: `None` when another holder has it. Never
    /// waits for it. An error means no lock can be taken here at all.
    fn acquire(&self, name: String) -> rdc::rt::BoxFuture<'static, Result<Option<HeldLock>>>;
}

/// [`EnvLocks`] in this process's memory: the native default, and the tests'
/// stand-in for the browser's lock manager. Clones share the held set.
#[derive(Clone, Default)]
pub struct MemLocks(Arc<Mutex<HashSet<String>>>);

struct MemHeld {
    held: Arc<Mutex<HashSet<String>>>,
    name: String,
}

impl Drop for MemHeld {
    fn drop(&mut self) {
        self.held.lock().unwrap().remove(&self.name);
    }
}

impl EnvLocks for MemLocks {
    fn acquire(&self, name: String) -> rdc::rt::BoxFuture<'static, Result<Option<HeldLock>>> {
        let free = self.0.lock().unwrap().insert(name.clone());
        let held = free.then(|| Box::new(MemHeld { held: self.0.clone(), name }) as HeldLock);
        Box::pin(async move { Ok(held) })
    }
}

#[cfg(target_arch = "wasm32")]
fn default_locks() -> Arc<dyn EnvLocks> {
    Arc::new(bindings::WebLocks)
}

#[cfg(not(target_arch = "wasm32"))]
fn default_locks() -> Arc<dyn EnvLocks> {
    Arc::new(MemLocks::default())
}

/// The Web Lock of a remote org: `rdc:<api_base>:<org_id>`, `api_base` as in
/// `rdc.toml` without a trailing slash. Named after the org, not the project
/// or the state key, so every tab that syncs one org excludes every other.
fn lock_name(api_base: &str, org_id: u64) -> String {
    format!("rdc:{}:{org_id}", api_base.trim_end_matches('/'))
}

/// `useDataStorageState`'s option names must all be known: an option rdc
/// does not read must not look as if it were applied.
pub fn check_state_option_keys(keys: impl IntoIterator<Item = String>) -> Result<()> {
    for k in keys {
        match k.as_str() {
            "key" | "chunkBytes" => {}
            "collection" => {
                return Err(anyhow!(
                    "unknown option 'collection': the state collection is fixed ({})",
                    rdc::state::data_storage_store::STATE_COLLECTION
                ));
            }
            _ => return Err(anyhow!("unknown option '{k}'")),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- the project

/// Marks an env as busy with a sync or watch; dropping it frees the env.
/// The in-memory stand-in for the on-disk `EnvLock`, which a non-native
/// store skips.
pub struct EnvClaim {
    busy: Arc<Mutex<HashSet<String>>>,
    env: String,
}

impl Drop for EnvClaim {
    fn drop(&mut self) {
        self.busy.lock().unwrap().remove(&self.env);
    }
}

/// A running watch's controls.
pub struct WatchControl {
    triggers: tokio::sync::mpsc::Sender<embed::CycleTrigger>,
    cancel: rdc::cli::sync::watch::CancelToken,
}

impl WatchControl {
    /// Run a cycle now, as a local file change would. Dropped when one is
    /// already queued.
    pub fn trigger(&self) {
        let _ = self.triggers.try_send(embed::CycleTrigger::FileEvent);
    }
    /// Stop the watch. A prompt it is waiting on reads as unanswered, so a
    /// stop never waits for the host to answer.
    pub fn stop(&self) {
        self.cancel.cancel();
    }
}

impl Drop for WatchControl {
    /// A handle freed from JS without `stop()` must not keep polling or keep
    /// the env claimed.
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// One project held in memory. Cheap to clone: clones share the tree.
#[derive(Clone)]
pub struct ProjectCore {
    store: Arc<MemStore>,
    root: PathBuf,
    name: Arc<Mutex<String>>,
    busy: Arc<Mutex<HashSet<String>>>,
    locks: Arc<dyn EnvLocks>,
    /// Set by [`Self::use_data_storage_state`]; `None` keeps the state in
    /// the tree.
    state: Arc<Mutex<Option<DataStorageStateOptions>>>,
}

impl ProjectCore {
    fn with_store(store: MemStore, name: String) -> Self {
        Self {
            store: Arc::new(store),
            root: PathBuf::from(ROOT),
            name: Arc::new(Mutex::new(name)),
            busy: Arc::default(),
            locks: default_locks(),
            state: Arc::default(),
        }
    }

    /// The same project over another lock manager.
    pub fn with_locks(mut self, locks: Arc<dyn EnvLocks>) -> Self {
        self.locks = locks;
        self
    }

    /// Keep each env's sync state (`.rdc/state/<env>.*`, `.rdc/conflicts/<env>/`)
    /// in Data Storage under `key` from the next `sync` or `watch` on.
    pub fn use_data_storage_state(&self, opts: DataStorageStateOptions) -> Result<()> {
        if opts.key.trim().is_empty() {
            return Err(anyhow!("useDataStorageState: `key` must not be empty."));
        }
        *self.state.lock().unwrap() = Some(opts);
        Ok(())
    }

    fn state_options(&self) -> Option<DataStorageStateOptions> {
        self.state.lock().unwrap().clone()
    }

    /// Hold the lock of the org `env` syncs with, or fail when another tab
    /// holds it.
    async fn lock_env(&self, env: &str, api_base: &str, org_id: u64) -> Result<HeldLock> {
        self.locks
            .acquire(lock_name(api_base, org_id))
            .await?
            .ok_or_else(|| anyhow!("env '{env}' is already being synced in another tab"))
    }

    /// An existing project, from its files keyed relative to the root.
    pub fn open(files: impl IntoIterator<Item = (String, Vec<u8>)>) -> Result<Self> {
        let store = MemStore::from_files(PathBuf::from(ROOT), files)
            .map_err(|e| anyhow!("invalid project file key: {}", strip_root(&e.to_string())))?;
        Ok(Self::with_store(store, String::new()))
    }

    /// A new project with one env: `rdc.toml`, its secrets and the scaffold.
    pub fn create(name: &str, first_env: &EnvInput) -> Result<Self> {
        let p = Self::with_store(MemStore::new(PathBuf::from(ROOT)), rdc::slug::slugify(name));
        p.run(|root| {
            embed::add_project(root, &first_env.into())?;
            scaffold(root, first_env)
        })
        .map_err(|e| anyhow!(user_message(&e)))?;
        Ok(p)
    }

    /// Run `f` on the tree.
    fn run<R>(&self, f: impl FnOnce(&Path) -> R) -> R {
        let store: Arc<dyn Store> = self.store.clone();
        rdc::vfs::scope_sync(store, || f(&self.root))
    }

    fn store(&self) -> Arc<dyn Store> {
        self.store.clone()
    }

    pub fn files(&self) -> BTreeMap<String, Vec<u8>> {
        self.store.files()
    }

    /// Called with the key and new bytes (`None` for a removal) after every
    /// write rdc or the host makes. Replaces any earlier callback.
    pub fn on_change(&self, f: Box<OnChange>) {
        self.store.set_on_change(f);
    }

    /// A host-side edit. Missing parent directories are created.
    pub fn write_file(&self, key: &str, bytes: &[u8]) -> Result<()> {
        self.run(|root| {
            let path = root.join(key);
            if let Some(parent) = path.parent() {
                rdc::vfs::create_dir_all(parent)?;
            }
            rdc::vfs::write(&path, bytes)
        })
        .map_err(|e| anyhow!("{key}: {e}"))
    }

    pub fn delete_file(&self, key: &str) -> Result<()> {
        self.run(|root| rdc::vfs::remove_file(root.join(key))).map_err(|e| anyhow!("{key}: {e}"))
    }

    fn summary_of(&self, info: embed::ProjectInfo) -> ProjectSummary {
        let name = self.name.lock().unwrap().clone();
        ProjectSummary {
            id: name.clone(),
            name,
            folder: String::new(),
            envs: info
                .envs
                .into_iter()
                .map(|e| EnvSummary {
                    name: e.name,
                    api_base: e.api_base,
                    org_id: e.org_id,
                    auth_kind: e.auth_kind.into(),
                    last_sync_unix: e.last_sync_unix,
                    file_count: e.file_count,
                })
                .collect(),
            error: info.error,
        }
    }

    /// `validate_existing_project`: the tree must be an rdc project.
    pub fn summary(&self) -> Result<ProjectSummary> {
        let info = self.run(embed::validate_project)?;
        Ok(self.summary_of(info))
    }

    pub fn edit_project(&self, input: &EditProjectInput) -> Result<ProjectSummary> {
        self.run(|root| {
            if !rdc::vfs::exists(root.join("rdc.toml")) {
                return Err(anyhow!("Project not found"));
            }
            embed::edit_env(root, &input.env, &embed::EditEnvInput {
                api_base: input.api_base.clone(),
                org_id: input.org_id,
                auth_kind: input.auth_kind.into(),
                token: input.token.clone(),
                username: input.username.clone(),
                password: input.password.clone(),
            })
        })?;
        *self.name.lock().unwrap() = rdc::slug::slugify(&input.name);
        self.summary()
    }

    /// Add an env and write its scaffold (the desktop app does that before
    /// its first sync; here the host may save the tree before one).
    pub fn add_env(&self, input: &EnvInput) -> Result<ProjectSummary> {
        self.run(|root| {
            embed::add_env(root, &input.into())?;
            scaffold(root, input)
        })?;
        self.summary()
    }

    /// `None` when `env` was the last one: the project is gone, and so is
    /// every file in the tree (the desktop app trashes the folder).
    ///
    /// Refused while `env` is being synced or watched (on disk, `EnvLock`
    /// refuses it; in memory nothing else would).
    pub fn remove_env(&self, env: &str) -> Result<Option<ProjectSummary>> {
        let _claim = self.claim(env)?;
        match self.run(|root| embed::remove_env(root, env))? {
            embed::RemovedEnv::Removed => self.summary().map(Some),
            embed::RemovedEnv::LastEnv => {
                for key in self.files().into_keys() {
                    self.delete_file(&key)?;
                }
                Ok(None)
            }
        }
    }

    /// Refused while `old` is being synced or watched: the running cycle
    /// would write `envs/<old>/` back. A rename to the same name is the
    /// edit dialog's no-op and needs no claim.
    pub fn rename_env(&self, old: &str, new: &str) -> Result<RenameEnvResult> {
        let _claim = if new.trim() == old { None } else { Some(self.claim(old)?) };
        let report = self.run(|root| embed::rename_env(root, old, new))?;
        Ok(RenameEnvResult {
            project: self.summary()?,
            follow_ups: report.follow_ups,
            warnings: report.warnings,
        })
    }

    /// Mark `env` busy, or fail if a sync or watch already holds it.
    pub fn claim(&self, env: &str) -> Result<EnvClaim> {
        if !self.busy.lock().unwrap().insert(env.to_string()) {
            return Err(anyhow!("\"{env}\" is already being synced or watched."));
        }
        Ok(EnvClaim { busy: self.busy.clone(), env: env.to_string() })
    }

    /// `env`'s api_base and org_id from `rdc.toml`.
    fn env_config(&self, env: &str) -> Result<(String, u64)> {
        let cfg = self.run(|root| rdc::config::ProjectConfig::load(&root.join("rdc.toml")))?;
        let ec = cfg
            .envs
            .get(env)
            .ok_or_else(|| anyhow!("This project has no \"{env}\" environment."))?;
        Ok((ec.api_base.clone(), ec.org_id))
    }

    /// The desktop's `sync_env`: scaffold, resolve the token (a silent
    /// re-login in password mode), run one cycle.
    pub async fn sync(
        self,
        env: String,
        opts: SyncOptions,
        log: Box<dyn Write + Send>,
        route: Arc<dyn embed::PromptRoute>,
    ) -> Result<CycleOutcome> {
        let _claim = self.claim(&env)?;
        let state_opts = self.state_options();
        let (api_base, org_id) = self.env_config(&env)?;
        let _lock = self.lock_env(&env, &api_base, org_id).await?;
        let root = self.root.clone();
        let store = self.store();
        let o = rdc::vfs::scope(store.clone(), async {
            rdc::cli::init::write_scaffold_files(&root, &env, &api_base, org_id)?;
            let token = rdc::secrets::resolve_token(&root, &env, &api_base).await?;
            let state = state_store(state_opts, &api_base, org_id, &token)?;
            embed::with_route(
                route,
                embed::sync_logged_in_with_state(store.clone(), state, &root, &env, &token, opts.into(), log),
            )
            .await
        })
        .await?;
        let file_count = self.run(|root| embed::count_files(&rdc::paths::Paths::for_env(root, env.as_str()).env_root()));
        Ok(CycleOutcome {
            items_pushed: o.items_pushed,
            items_pulled: o.items_pulled,
            conflicts: o.conflicts,
            remote_deletes_resolved: o.remote_deletes_resolved,
            file_count,
        })
    }

    /// The desktop's `watch_env`. The env is claimed now, so a second call
    /// fails at once; the returned future runs the watch until
    /// [`WatchControl::stop`] and frees the env when it ends.
    ///
    /// `cancel` is the watch's stop; give the route the same token (see
    /// [`log_and_route`]).
    pub fn watch(
        &self,
        env: String,
        poll: Option<Duration>,
        log: Box<dyn Write + Send>,
        route: Arc<dyn embed::PromptRoute>,
        on_idle: Option<Arc<dyn Fn(Option<u64>) + Send + Sync>>,
        cancel: rdc::cli::sync::watch::CancelToken,
    ) -> Result<(WatchControl, impl Future<Output = Result<()>> + 'static)> {
        let claim = self.claim(&env)?;
        let state_opts = self.state_options();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let control = WatchControl { triggers: tx, cancel: cancel.clone() };
        let this = self.clone();
        let fut = async move {
            let _claim = claim;
            let (api_base, org_id) = this.env_config(&env)?;
            let _lock = this.lock_env(&env, &api_base, org_id).await?;
            let root = this.root.clone();
            let store = this.store();
            let token = rdc::vfs::scope(store.clone(), async {
                rdc::cli::init::write_scaffold_files(&root, &env, &api_base, org_id)?;
                rdc::secrets::resolve_token(&root, &env, &api_base).await
            })
            .await?;
            let state = state_store(state_opts, &api_base, org_id, &token)?;
            embed::watch_logged_in_with_state(
                store, state, &root, &env, &api_base, token, poll, log, route, cancel, on_idle, Some(rx),
            )
            .await
        };
        Ok((control, fut))
    }

    /// An error as the host should see it: paths relative to the tree.
    pub fn message(&self, e: &anyhow::Error) -> String {
        user_message(e)
    }
}

/// Where a run keeps `env`'s sync state: Data Storage, on the env's cluster
/// with the run's token, when `opts` is set; else the tree itself.
fn state_store(
    opts: Option<DataStorageStateOptions>,
    api_base: &str,
    org_id: u64,
    token: &str,
) -> Result<Arc<dyn StateStore>> {
    let Some(o) = opts else {
        return Ok(Arc::new(FsStateStore));
    };
    let base = rdc::config::EnvConfig { api_base: api_base.to_string(), org_id }.data_storage_base();
    let client = rdc::api::DataStorageClient::new(base, token.to_string())?;
    Ok(Arc::new(rdc::state::data_storage_store::DataStorageStateStore::new(
        client,
        rdc::state::data_storage_store::STATE_COLLECTION.to_string(),
        o.key,
        o.chunk_bytes,
    )))
}

/// `e` as the host should see it: paths relative to the tree, the virtual
/// root never named. Free so the constructors, which have no project yet,
/// can scrub their own errors.
fn user_message(e: &anyhow::Error) -> String {
    strip_root(&format!("{e:#}"))
}

fn strip_root(s: &str) -> String {
    s.replace(&format!("{ROOT}/"), "").replace(ROOT, "the project")
}

/// The files `rdc init` drops next to an env (`.gitlab-ci.yml`, README, …).
fn scaffold(root: &Path, env: &EnvInput) -> Result<()> {
    rdc::cli::init::write_scaffold_files(
        root,
        env.name.trim(),
        env.api_base.trim_end_matches('/'),
        env.org_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev() -> EnvInput {
        EnvInput {
            name: "dev".into(),
            api_base: "https://d.test/api/v1".into(),
            org_id: 1,
            auth_kind: AuthKind::Token,
            token: Some("tok".into()),
            username: None,
            password: None,
        }
    }

    #[test]
    fn dropping_a_watch_control_cancels_its_token() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let cancel = rdc::cli::sync::watch::CancelToken::new();
        let control = WatchControl { triggers: tx, cancel: cancel.clone() };
        assert!(!cancel.is_cancelled());
        drop(control);
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn create_then_summary() {
        let p = ProjectCore::create("acme", &dev()).unwrap();
        let files = p.files();
        for key in ["rdc.toml", ".gitlab-ci.yml", "README.md"] {
            assert!(files.contains_key(key), "missing {key}: {:?}", files.keys());
        }
        let s = p.summary().unwrap();
        assert_eq!(s.envs.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["dev"]);
    }

    #[test]
    fn rename_env_moves_tree() {
        let p = ProjectCore::create("acme", &dev()).unwrap();
        p.write_file("envs/dev/queues/x.json", b"{}").unwrap();
        p.rename_env("dev", "test").unwrap();
        let files = p.files();
        assert!(!files.keys().any(|k| k.starts_with("envs/dev/")), "{:?}", files.keys());
        assert!(files.contains_key("envs/test/queues/x.json"), "{:?}", files.keys());
    }

    #[test]
    fn on_change_sees_scaffold_writes() {
        let p = ProjectCore::create("acme", &dev()).unwrap();
        let before: std::collections::BTreeSet<String> = p.files().into_keys().collect();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
        let sink = seen.clone();
        p.on_change(Box::new(move |k, _| {
            sink.lock().unwrap().insert(k.to_string());
        }));
        let mut prod = dev();
        prod.name = "prod".into();
        p.add_env(&prod).unwrap();
        let after: std::collections::BTreeSet<String> = p.files().into_keys().collect();
        let new: Vec<&String> = after.difference(&before).collect();
        assert!(!new.is_empty());
        let seen = seen.lock().unwrap();
        for k in new {
            assert!(seen.contains(k), "{k} was written but not reported");
        }
    }

    #[test]
    fn a_second_sync_or_watch_on_the_same_env_is_rejected() {
        let p = ProjectCore::create("acme", &dev()).unwrap();
        let claim = p.claim("dev").unwrap();
        assert!(p.clone().claim("dev").is_err(), "clones share the busy set");
        assert!(p.claim("prod").is_ok());
        drop(claim);
        assert!(p.claim("dev").is_ok(), "dropping the claim frees the env");
    }

    fn sample_prompt() -> embed::Prompt {
        embed::Prompt {
            kind: embed::PromptKind::DeleteGate,
            question: "Proceed? ".into(),
            keys: vec![
                embed::PromptKey::new('y', "yes"),
                embed::PromptKey::new('e', "edit"),
                embed::PromptKey::new('h', "hunk-by-hunk"),
                embed::PromptKey::new('n', "no"),
            ],
        }
    }

    #[test]
    fn prompts_never_offer_or_accept_e_or_h() {
        let (dto, allowed) = offer(&sample_prompt());
        assert_eq!(dto.kind, PromptKind::DeleteGate);
        let keys: Vec<&str> = dto.choices.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["y", "n"]);
        assert!(!dto.question.contains("edit"), "{}", dto.question);
        assert_eq!(accept(Some("y".into()), &allowed), Some("y".into()));
        assert_eq!(accept(Some("e".into()), &allowed), None);
        assert_eq!(accept(None, &allowed), None);
    }

    #[test]
    fn a_prompt_drops_the_unterminated_question_from_the_log() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        let (mut log, route) = log_and_route(
            move |l| sink.lock().unwrap().push(l.to_string()),
            Box::new(|_| Box::pin(async { Some("y".to_string()) })),
            None,
        );
        log.write_all(b"[y] yes  [n] no > ").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert_eq!(rt.block_on(route.ask(&sample_prompt())), Some("y".into()));
        log.write_all(b"12:01:14 pull  queues/invoices\n").unwrap();
        assert_eq!(*lines.lock().unwrap(), vec!["12:01:14 pull  queues/invoices"]);
    }

    #[test]
    fn a_stopped_watch_answers_a_pending_prompt_with_none() {
        let cancel = rdc::cli::sync::watch::CancelToken::new();
        let (_log, route) = log_and_route(
            |_| {},
            Box::new(|_| Box::pin(futures::future::pending())),
            Some(cancel.clone()),
        );
        cancel.cancel();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert_eq!(rt.block_on(route.ask(&sample_prompt())), None);
    }

    #[test]
    fn removing_the_last_env_empties_the_tree() {
        let p = ProjectCore::create("acme", &dev()).unwrap();
        assert!(p.remove_env("dev").unwrap().is_none());
        assert!(p.files().is_empty(), "{:?}", p.files().keys());
    }

    #[test]
    fn host_writes_cannot_leave_the_tree() {
        let p = ProjectCore::create("acme", &dev()).unwrap();
        assert!(p.write_file("../escape.txt", b"x").is_err());
        assert!(p.write_file("/etc/x", b"x").is_err());
        p.write_file("envs/dev/a/b.json", b"{}").unwrap();
        assert_eq!(p.files()["envs/dev/a/b.json"], b"{}");
    }

    #[test]
    fn opened_files_round_trip_and_summarise() {
        let created = ProjectCore::create("acme", &dev()).unwrap();
        let p = ProjectCore::open(created.files()).unwrap();
        assert_eq!(p.files(), created.files());
        let s = p.summary().unwrap();
        assert_eq!(s.envs[0].name, "dev");
        assert_eq!(s.envs[0].api_base, "https://d.test/api/v1");
        assert!(ProjectCore::open([("../x".to_string(), vec![])]).is_err());
    }

    #[test]
    fn errors_name_paths_relative_to_the_tree() {
        let p = ProjectCore::open(Vec::new()).unwrap();
        let e = p.summary().unwrap_err();
        let m = p.message(&e);
        assert!(!m.contains(ROOT), "{m}");
    }

    #[test]
    fn user_message_strips_the_virtual_root() {
        let e = anyhow!("reading {ROOT}/envs/dev/x.json failed under {ROOT}");
        assert_eq!(user_message(&e), "reading envs/dev/x.json failed under the project");
    }

    #[test]
    fn rename_or_remove_of_a_busy_env_is_refused() {
        let p = ProjectCore::create("acme", &dev()).unwrap();
        let mut prod = dev();
        prod.name = "prod".into();
        p.add_env(&prod).unwrap();
        p.write_file("envs/dev/queues/x.json", b"{}").unwrap();
        let before = p.files();
        let claim = p.claim("dev").unwrap();
        assert!(p.rename_env("dev", "test").is_err());
        assert!(p.remove_env("dev").is_err());
        assert_eq!(p.files(), before, "a refused call leaves the tree as it was");
        drop(claim);
        // The calls free the env again, so a later sync can claim it.
        p.rename_env("dev", "test").unwrap();
        assert!(p.claim("test").is_ok() && p.claim("dev").is_ok());
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;
    use rdc::cli::sync::watch::CancelToken;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockBuilder, MockServer, ResponseTemplate};

    const DS: &str = "/svc/data-storage/api/v1";

    /// `m`, only for requests carrying `token` when one is given. The API
    /// sends `token <t>`, Data Storage `Bearer <t>`.
    fn with_token(m: MockBuilder, token: Option<&str>, scheme: &str) -> MockBuilder {
        match token {
            Some(t) => m.and(header("authorization", format!("{scheme} {t}").as_str())),
            None => m,
        }
    }

    /// An empty org and an empty Data Storage (no manifest yet, every write
    /// accepted), answering only `token` when one is given.
    async fn mount_world(server: &MockServer, token: Option<&str>) {
        with_token(Mock::given(method("GET")).and(path("/organizations/1")), token, "token")
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": 1, "name": "test-org", "url": format!("{}/organizations/1", server.uri())
            })))
            .mount(server)
            .await;
        for kind in [
            "workspaces", "queues", "schemas", "inboxes", "hooks", "rules", "labels", "engines",
            "engine_fields", "workflows", "workflow_steps", "email_templates", "saved_views",
        ] {
            with_token(Mock::given(method("GET")).and(path(format!("/{kind}"))), token, "token")
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [], "pagination": {"next": null, "total_pages": 1, "total": 0}
                })))
                .mount(server)
                .await;
        }
        with_token(Mock::given(method("POST")).and(path(format!("{DS}/data/find"))), token, "Bearer")
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"code": "ok", "message": "", "result": []})))
            .mount(server)
            .await;
        with_token(Mock::given(method("POST")).and(path(format!("{DS}/data/replace_one"))), token, "Bearer")
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": "ok", "message": "", "result": {"matched_count": 0, "modified_count": 0, "upserted_id": "x"}
            })))
            .mount(server)
            .await;
        with_token(Mock::given(method("POST")).and(path(format!("{DS}/data/insert_many"))), token, "Bearer")
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"code": "ok", "message": ""})))
            .mount(server)
            .await;
    }

    fn token_env(server: &MockServer) -> EnvInput {
        EnvInput {
            name: "dev".into(),
            api_base: server.uri(),
            org_id: 1,
            auth_kind: AuthKind::Token,
            token: Some("tok".into()),
            username: None,
            password: None,
        }
    }

    fn project_on(server: &MockServer, locks: &MemLocks) -> ProjectCore {
        ProjectCore::create("acme", &token_env(server)).unwrap().with_locks(Arc::new(locks.clone()))
    }

    /// The lock every tab syncing `server`'s org 1 takes.
    fn lock_of(server: &MockServer) -> String {
        format!("rdc:{}:1", server.uri())
    }

    fn quiet_route() -> Arc<dyn embed::PromptRoute> {
        log_and_route(|_| {}, Box::new(|_| Box::pin(async { None })), None).1
    }

    async fn sync(p: &ProjectCore, env: &str) -> Result<CycleOutcome> {
        let opts = SyncOptions { interactive: false, no_push: true, ..SyncOptions::default() };
        p.clone().sync(env.into(), opts, Box::new(std::io::sink()), quiet_route()).await
    }

    /// A watch of `dev` without polling: its log lines, and one message per
    /// completed cycle after the initial reconcile.
    #[allow(clippy::type_complexity)]
    fn start_watch(
        p: &ProjectCore,
    ) -> (WatchControl, impl Future<Output = Result<()>>, UnboundedReceiver<String>, UnboundedReceiver<()>) {
        let (line_tx, lines) = unbounded_channel();
        let (idle_tx, idles) = unbounded_channel();
        let cancel = CancelToken::new();
        let (log, route) = log_and_route(
            move |l| {
                let _ = line_tx.send(l.to_string());
            },
            Box::new(|_| Box::pin(async { None })),
            Some(cancel.clone()),
        );
        let on_idle: Arc<dyn Fn(Option<u64>) + Send + Sync> = Arc::new(move |_| {
            let _ = idle_tx.send(());
        });
        let (control, fut) = p.watch("dev".into(), None, log, route, Some(on_idle), cancel).unwrap();
        (control, fut, lines, idles)
    }

    /// Wait for the watch's initial reconcile to end.
    async fn started(lines: &mut UnboundedReceiver<String>) {
        while let Some(l) = lines.recv().await {
            if l.contains("start envs/dev") {
                return;
            }
        }
        panic!("the watch ended before it started");
    }

    fn state_requests(requests: &[wiremock::Request]) -> Vec<&wiremock::Request> {
        requests
            .iter()
            .filter(|r| {
                serde_json::from_slice::<serde_json::Value>(&r.body)
                    .is_ok_and(|b| b["collectionName"] == rdc::state::data_storage_store::STATE_COLLECTION)
            })
            .collect()
    }

    fn ds_state(key: &str) -> DataStorageStateOptions {
        DataStorageStateOptions { key: key.into(), chunk_bytes: DEFAULT_CHUNK_BYTES }
    }

    #[tokio::test]
    async fn sync_uses_data_storage_state_when_set() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let p = project_on(&server, &MemLocks::default());
        p.use_data_storage_state(ds_state("acme-key")).unwrap();

        sync(&p, "dev").await.expect("the sync succeeds");

        let requests = server.received_requests().await.unwrap();
        assert!(!state_requests(&requests).is_empty(), "no request reached the state collection");
        let manifest = requests
            .iter()
            .filter(|r| r.url.path() == format!("{DS}/data/insert_many"))
            .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
            .find(|b| b["documents"][0]["_id"] == "acme-key/dev")
            .expect("the run published a manifest for acme-key/dev");
        assert!(
            manifest["documents"][0]["files"].get(".rdc/state/dev.lock.json").is_some(),
            "the lockfile is in the manifest: {manifest}"
        );
    }

    #[test]
    fn data_storage_state_options_default_the_chunk_size_and_need_a_key() {
        let o: DataStorageStateOptions = serde_json::from_value(serde_json::json!({"key": "k"})).unwrap();
        assert_eq!((o.key.as_str(), o.chunk_bytes), ("k", 524_288));
        let o: DataStorageStateOptions =
            serde_json::from_value(serde_json::json!({"key": "k", "chunkBytes": 10})).unwrap();
        assert_eq!(o.chunk_bytes, 10);
        let p = ProjectCore::open(Vec::new()).unwrap();
        assert!(p.use_data_storage_state(ds_state(" ")).is_err(), "an empty key is refused");
    }

    #[test]
    fn an_unknown_state_option_is_refused() {
        let keys = |ks: &[&str]| check_state_option_keys(ks.iter().map(|k| k.to_string()));
        assert!(keys(&["key", "chunkBytes"]).is_ok());
        assert_eq!(
            keys(&["key", "collection"]).unwrap_err().to_string(),
            "unknown option 'collection': the state collection is fixed (__rdc_state)"
        );
        assert_eq!(keys(&["key", "chunk_bytes"]).unwrap_err().to_string(), "unknown option 'chunk_bytes'");
    }

    #[tokio::test]
    async fn default_stays_in_memory() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let p = project_on(&server, &MemLocks::default());

        sync(&p, "dev").await.expect("the sync succeeds");

        let requests = server.received_requests().await.unwrap();
        assert!(state_requests(&requests).is_empty(), "the state collection was touched");
        assert!(p.files().contains_key(".rdc/state/dev.lock.json"), "the state stays in the tree");
    }

    #[tokio::test]
    async fn a_lock_held_in_another_tab_refuses_the_sync_and_the_watch() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let locks = MemLocks::default();
        let p = project_on(&server, &locks);
        p.use_data_storage_state(ds_state("acme-key")).unwrap();
        // Another tab: its own project over the same lock manager.
        let held = locks.acquire(lock_of(&server)).await.unwrap().expect("free");

        let err = sync(&p, "dev").await.expect_err("the env is locked elsewhere");
        assert_eq!(err.to_string(), "env 'dev' is already being synced in another tab");
        assert!(server.received_requests().await.unwrap().is_empty(), "nothing ran");

        let (_control, fut, _lines, _idles) = start_watch(&p);
        let err = fut.await.expect_err("the watch is refused too");
        assert_eq!(err.to_string(), "env 'dev' is already being synced in another tab");
        drop(held);
    }

    #[test]
    fn the_lock_is_named_after_the_remote_org() {
        assert_eq!(lock_name("https://d.test/api/v1/", 7), "rdc:https://d.test/api/v1:7");
        assert_ne!(lock_name("https://d.test/api/v1", 7), lock_name("https://d.test/api/v1", 8));
    }

    #[tokio::test]
    async fn a_data_storage_tab_and_a_plain_tab_on_one_org_exclude_each_other() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let locks = MemLocks::default();
        let with_state = project_on(&server, &locks);
        with_state.use_data_storage_state(ds_state("acme-key")).unwrap();
        let plain = ProjectCore::open(with_state.files()).unwrap().with_locks(Arc::new(locks.clone()));

        // Hold the plain tab's sync open at its first request.
        let (_control, fut, mut lines, _idles) = start_watch(&plain);
        let probe = async {
            started(&mut lines).await;
            let err = sync(&with_state, "dev").await.expect_err("the plain tab holds the org");
            assert_eq!(err.to_string(), "env 'dev' is already being synced in another tab");
            _control.stop();
        };
        let (watched, ()) = tokio::join!(fut, probe);
        watched.expect("the watch stops cleanly");
        sync(&with_state, "dev").await.expect("free again once the watch ended");

        // Another org on the same cluster is another lock.
        let mut other = token_env(&server);
        other.name = "prod".into();
        other.org_id = 2;
        plain.add_env(&other).unwrap();
        let held = locks.acquire(lock_of(&server)).await.unwrap().expect("free");
        assert!(locks.acquire(lock_name(&server.uri(), 2)).await.unwrap().is_some(), "org 2 is not org 1");
        drop(held);
    }

    #[tokio::test]
    async fn the_lock_is_released_after_a_sync_resolves_and_after_it_rejects() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let locks = MemLocks::default();
        let p = project_on(&server, &locks);

        sync(&p, "dev").await.expect("the sync succeeds");
        assert!(locks.acquire(lock_of(&server)).await.unwrap().is_some(), "released after resolve");

        // Fails after the lock is taken: the env has no credentials left.
        for key in p.files().into_keys().filter(|k| k.starts_with("secrets/")) {
            p.delete_file(&key).unwrap();
        }
        sync(&p, "dev").await.expect_err("the sync fails");
        assert!(locks.acquire(lock_of(&server)).await.unwrap().is_some(), "released after reject");
    }

    #[tokio::test]
    async fn a_watch_releases_its_lock_after_stop() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let locks = MemLocks::default();
        let p = project_on(&server, &locks);
        let (control, fut, mut lines, _idles) = start_watch(&p);
        let probe = async {
            started(&mut lines).await;
            assert!(locks.acquire(lock_of(&server)).await.unwrap().is_none(), "held while watching");
            control.stop();
        };
        let (watched, ()) = tokio::join!(fut, probe);
        watched.expect("the watch stops cleanly");
        assert!(locks.acquire(lock_of(&server)).await.unwrap().is_some(), "released after stop");
    }

    #[tokio::test]
    async fn a_watch_with_data_storage_state_survives_a_401() {
        let server = MockServer::start().await;
        let login = |key: &str| {
            Mock::given(method("POST"))
                .and(path("/auth/login"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "key": key })))
        };
        login("t1").mount(&server).await;
        mount_world(&server, Some("t1")).await;
        let env = EnvInput {
            auth_kind: AuthKind::Password,
            token: None,
            username: Some("u".into()),
            password: Some("p".into()),
            ..token_env(&server)
        };
        let p = ProjectCore::create("acme", &env).unwrap().with_locks(Arc::new(MemLocks::default()));
        p.use_data_storage_state(ds_state("acme-key")).unwrap();

        let (control, fut, mut lines, mut idles) = start_watch(&p);
        let drive = async {
            started(&mut lines).await;
            // `t1` is revoked: it now gets 401 everywhere; a re-login hands out `t2`.
            server.reset().await;
            for revoked in ["token t1", "Bearer t1"] {
                Mock::given(header("authorization", revoked))
                    .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({"detail": "expired"})))
                    .mount(&server)
                    .await;
            }
            login("t2").mount(&server).await;
            mount_world(&server, Some("t2")).await;
            control.trigger();
            idles.recv().await;
            // A later cycle keeps `t2` instead of 401ing on `t1` again.
            control.trigger();
            idles.recv().await;
            control.stop();
        };
        let (watched, ()) = tokio::join!(fut, drive);
        watched.expect("the watch survives the 401");

        let requests = server.received_requests().await.unwrap();
        let relogin = requests.iter().position(|r| r.url.path() == "/auth/login").expect("a re-login");
        assert_eq!(
            requests.iter().filter(|r| r.url.path() == "/auth/login").count(),
            1,
            "a cycle after the retry must not log in again"
        );
        let after = state_requests(&requests[relogin..]);
        assert!(!after.is_empty(), "the retried cycle reached the state collection");
        for r in after {
            assert_eq!(
                r.headers.get("authorization").and_then(|v| v.to_str().ok()),
                Some("Bearer t2"),
                "{} after the re-login",
                r.url.path()
            );
        }
    }
}
