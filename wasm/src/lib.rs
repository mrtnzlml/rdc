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
}

impl ProjectCore {
    fn with_store(store: MemStore, name: String) -> Self {
        Self {
            store: Arc::new(store),
            root: PathBuf::from(ROOT),
            name: Arc::new(Mutex::new(name)),
            busy: Arc::default(),
        }
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
        let (api_base, org_id) = self.env_config(&env)?;
        let root = self.root.clone();
        let store = self.store();
        let o = rdc::vfs::scope(store.clone(), async {
            rdc::cli::init::write_scaffold_files(&root, &env, &api_base, org_id)?;
            let token = rdc::secrets::resolve_token(&root, &env, &api_base).await?;
            embed::with_route(route, embed::sync_logged_in(store.clone(), &root, &env, &token, opts.into(), log))
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
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let control = WatchControl { triggers: tx, cancel: cancel.clone() };
        let this = self.clone();
        let fut = async move {
            let _claim = claim;
            let (api_base, org_id) = this.env_config(&env)?;
            let root = this.root.clone();
            let store = this.store();
            let token = rdc::vfs::scope(store.clone(), async {
                rdc::cli::init::write_scaffold_files(&root, &env, &api_base, org_id)?;
                rdc::secrets::resolve_token(&root, &env, &api_base).await
            })
            .await?;
            embed::watch_logged_in(
                store, &root, &env, &api_base, token, poll, log, route, cancel, on_idle, Some(rx),
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
