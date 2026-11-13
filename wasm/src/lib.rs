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
use std::collections::{BTreeMap, BTreeSet, HashSet};
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
/// file count, a dry run's plan and what the cycle did not write.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CycleOutcome {
    pub items_pushed: usize,
    pub items_pulled: usize,
    pub conflicts: usize,
    pub remote_deletes_resolved: usize,
    pub file_count: u64,
    /// Set only by a dry run.
    pub plan: Option<SyncPlan>,
    pub skipped: Vec<SkippedItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanDirection {
    Push,
    Pull,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanChange {
    Create,
    Update,
    Delete,
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanPrompt {
    DeleteGate,
    Conflict,
    RemoteDelete,
}

/// One row of a dry run's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanItem {
    pub kind: String,
    pub slug: String,
    pub direction: PlanDirection,
    pub change: PlanChange,
    pub prompt: Option<PlanPrompt>,
    pub note: Option<String>,
}

/// A crashed run's create the dry run's recovery settled: `id` to adopt, or
/// `null` when it made nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanRecovery {
    pub kind: String,
    pub slug: String,
    pub id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncPlan {
    pub items: Vec<PlanItem>,
    pub recovered: Vec<PlanRecovery>,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SkipReason {
    DeleteGateDeclined,
    DeleteDriftSkipped,
    PushDriftSkipped,
    ConflictSkipped,
    RemoteDeleteSkipped,
    MdhIndexDropDeclined,
    MdhRowDeleteDeclined,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedItem {
    pub kind: String,
    pub slug: String,
    pub reason: SkipReason,
}

// No catch-all arms below, as for `kind_of`: a new core variant must fail to
// compile here rather than reach the host mislabelled.
impl From<rdc::cli::sync::plan::SyncPlan> for SyncPlan {
    fn from(p: rdc::cli::sync::plan::SyncPlan) -> Self {
        use rdc::cli::sync::plan as core;
        let items = p
            .items
            .into_iter()
            .map(|i| PlanItem {
                kind: i.kind,
                slug: i.slug,
                direction: match i.direction {
                    core::PlanDirection::Push => PlanDirection::Push,
                    core::PlanDirection::Pull => PlanDirection::Pull,
                    core::PlanDirection::Both => PlanDirection::Both,
                },
                change: match i.change {
                    core::PlanChange::Create => PlanChange::Create,
                    core::PlanChange::Update => PlanChange::Update,
                    core::PlanChange::Delete => PlanChange::Delete,
                    core::PlanChange::Conflict => PlanChange::Conflict,
                },
                prompt: i.prompt.map(|p| match p {
                    core::PlanPrompt::DeleteGate => PlanPrompt::DeleteGate,
                    core::PlanPrompt::Conflict => PlanPrompt::Conflict,
                    core::PlanPrompt::RemoteDelete => PlanPrompt::RemoteDelete,
                }),
                note: i.note,
            })
            .collect();
        let recovered =
            p.recovered.into_iter().map(|r| PlanRecovery { kind: r.kind, slug: r.slug, id: r.id }).collect();
        Self { items, recovered, problems: p.problems }
    }
}

impl From<rdc::cli::sync::plan::SkippedItem> for SkippedItem {
    fn from(i: rdc::cli::sync::plan::SkippedItem) -> Self {
        use rdc::cli::sync::plan::SkipReason as R;
        Self {
            kind: i.kind,
            slug: i.slug,
            reason: match i.reason {
                R::DeleteGateDeclined => SkipReason::DeleteGateDeclined,
                R::DeleteDriftSkipped => SkipReason::DeleteDriftSkipped,
                R::PushDriftSkipped => SkipReason::PushDriftSkipped,
                R::ConflictSkipped => SkipReason::ConflictSkipped,
                R::RemoteDeleteSkipped => SkipReason::RemoteDeleteSkipped,
                R::MdhIndexDropDeclined => SkipReason::MdhIndexDropDeclined,
                R::MdhRowDeleteDeclined => SkipReason::MdhRowDeleteDeclined,
            },
        }
    }
}

/// A `--carry` group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CarryGroup {
    ScoreThresholds,
    EmailPrefixes,
    Automation,
    All,
}

impl From<CarryGroup> for embed::CarryGroup {
    fn from(g: CarryGroup) -> Self {
        match g {
            CarryGroup::ScoreThresholds => embed::CarryGroup::ScoreThresholds,
            CarryGroup::EmailPrefixes => embed::CarryGroup::EmailPrefixes,
            CarryGroup::Automation => embed::CarryGroup::Automation,
            CarryGroup::All => embed::CarryGroup::All,
        }
    }
}

/// `migrate`'s options: `rdc migrate`'s flags. Every field may be left out;
/// the defaults are an unflagged `rdc migrate`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct MigrateOptions {
    pub dry_run: bool,
    pub mirror: bool,
    /// Needs `mirror`, as `--allow-recreate` needs `--mirror`.
    pub allow_recreate: bool,
    pub only: Vec<String>,
    pub carry: Vec<CarryGroup>,
}

/// `migrate`'s option names must all be known: an option rdc does not read must not look applied.
pub fn check_migrate_option_keys(keys: impl IntoIterator<Item = String>) -> Result<()> {
    for k in keys {
        match k.as_str() {
            "dryRun" | "mirror" | "allowRecreate" | "only" | "carry" => {}
            _ => return Err(anyhow!("unknown option '{k}'")),
        }
    }
    Ok(())
}

impl TryFrom<MigrateOptions> for embed::EmbedMigrateOptions {
    type Error = anyhow::Error;
    fn try_from(o: MigrateOptions) -> Result<Self> {
        if o.allow_recreate && !o.mirror {
            return Err(anyhow!("migrate: `allowRecreate` needs `mirror`."));
        }
        let groups: Vec<embed::CarryGroup> = o.carry.into_iter().map(Into::into).collect();
        Ok(Self {
            mirror: if o.mirror {
                embed::MirrorMode::Mirror { allow_recreate: o.allow_recreate }
            } else {
                embed::MirrorMode::Additive
            },
            dry_run: o.dry_run,
            only: o.only,
            carry: embed::Carry::from_groups(&groups),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FileChange {
    Create,
    Update,
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ObjectChange {
    Create,
    Update,
    Unchanged,
    Delete,
}

/// One file a migrate considered, by project key, and the target object it
/// belongs to (`kind`/`slug` as the sync plan and `skipped` name it; `null`
/// for a file of no object).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrateFile {
    pub source: String,
    pub target: String,
    pub change: FileChange,
    pub kind: Option<String>,
    pub slug: Option<String>,
}

/// One target object, by its target slug, and what this run did to its
/// files in the target tree (`cli::migrate::ObjectChange`): `create` = the
/// tree had no file for it, `update` = its files changed, `unchanged`,
/// `delete` = `mirror` removed it. Not what the next `sync` sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrateObject {
    pub kind: String,
    pub slug: String,
    pub change: ObjectChange,
}

/// A target file `mirror` pruned, by project key, and the object it belonged
/// to, as [`MigrateFile`] names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigratePruned {
    pub target: String,
    pub kind: Option<String>,
    pub slug: Option<String>,
}

/// What a migrate changed, or under `dryRun` would change. Paths are
/// project keys (`envs/<env>/...`), as `files()` names them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateResult {
    pub dry_run: bool,
    pub source: String,
    pub target: String,
    pub files: Vec<MigrateFile>,
    pub objects: Vec<MigrateObject>,
    pub pruned: Vec<MigratePruned>,
    pub pruned_dirs: Vec<String>,
    pub skipped: Vec<String>,
    pub warnings: Vec<String>,
    pub creates: usize,
    pub updates: usize,
    pub unchanged: usize,
    pub deletes: usize,
    pub files_changed: usize,
    pub renamed: usize,
}

/// `rel` (relative to `envs/<env>/`) as a project key.
fn env_key(env: &str, rel: &Path) -> String {
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    format!("envs/{env}/{}", parts.join("/"))
}

impl MigrateResult {
    fn from_report(src: &str, tgt: &str, r: embed::MigrateReport) -> Self {
        let objects: Vec<MigrateObject> = r
            .objects
            .into_iter()
            .map(|o| MigrateObject {
                kind: o.kind,
                slug: o.slug,
                change: match o.change {
                    embed::ObjectChange::Create => ObjectChange::Create,
                    embed::ObjectChange::Update => ObjectChange::Update,
                    embed::ObjectChange::Unchanged => ObjectChange::Unchanged,
                    embed::ObjectChange::Delete => ObjectChange::Delete,
                },
            })
            .collect();
        let count = |c: ObjectChange| objects.iter().filter(|o| o.change == c).count();
        Self {
            dry_run: r.dry_run,
            source: src.to_string(),
            target: tgt.to_string(),
            files: r
                .files
                .iter()
                .map(|f| MigrateFile {
                    source: env_key(src, &f.source),
                    target: env_key(tgt, &f.target),
                    change: match f.change {
                        embed::FileOutcome::Created => FileChange::Create,
                        embed::FileOutcome::Updated => FileChange::Update,
                        embed::FileOutcome::Unchanged => FileChange::Unchanged,
                    },
                    kind: f.object.as_ref().map(|(k, _)| k.clone()),
                    slug: f.object.as_ref().map(|(_, s)| s.clone()),
                })
                .collect(),
            creates: count(ObjectChange::Create),
            updates: count(ObjectChange::Update),
            unchanged: count(ObjectChange::Unchanged),
            deletes: count(ObjectChange::Delete),
            objects,
            pruned: r
                .pruned
                .iter()
                .map(|p| MigratePruned {
                    target: env_key(tgt, &p.target),
                    kind: p.object.as_ref().map(|(k, _)| k.clone()),
                    slug: p.object.as_ref().map(|(_, s)| s.clone()),
                })
                .collect(),
            pruned_dirs: r.pruned_dirs.iter().map(|p| env_key(tgt, p)).collect(),
            skipped: r.skipped.iter().map(|p| env_key(src, p)).collect(),
            warnings: r.warnings,
            files_changed: r.files_changed,
            renamed: r.renamed,
        }
    }
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
    /// Which of several remote objects a crashed run's create made. Its
    /// `question` is the whole question.
    RecoverCreate,
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
        K::RecoverCreate => PromptKind::RecoverCreate,
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
    // A recovery prompt carries its whole question; every other kind's is the
    // menu line, rebuilt from the keys offered here.
    let question = match prompt.kind {
        embed::PromptKind::RecoverCreate => prompt.question.clone(),
        _ => embed::menu_one_line(&offered),
    };
    let dto = Prompt {
        kind: kind_of(prompt.kind),
        question,
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

/// The host's storage of each env's state snapshot (`useStateStore`).
pub use rdc::state::snapshot_store::SnapshotBackend;

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

/// `useStateStore`'s object must carry exactly `load` and `save`, both
/// functions: a member rdc does not call must not look as if it were used.
pub fn check_state_store_keys(keys: impl IntoIterator<Item = (String, bool)>) -> Result<()> {
    let mut seen = BTreeSet::new();
    for (k, is_fn) in keys {
        match k.as_str() {
            "load" | "save" if is_fn => {
                seen.insert(k);
            }
            "load" | "save" => return Err(anyhow!("useStateStore: `{k}` must be a function")),
            _ => return Err(anyhow!("useStateStore: unknown member '{k}'")),
        }
    }
    for k in ["load", "save"] {
        if !seen.contains(k) {
            return Err(anyhow!("useStateStore: `{k}` is missing"));
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
    /// Set by [`Self::use_state_store`]; `None` keeps the state in the tree.
    state: Arc<Mutex<Option<Arc<dyn SnapshotBackend>>>>,
    /// Per env, the store its runs use, kept for the project's life: it
    /// remembers the snapshot the tree last agreed with, which decides what
    /// its next hydrate keeps (see `SnapshotStateStore`). Emptied by
    /// [`Self::use_state_store`].
    stores: Arc<Mutex<BTreeMap<String, Arc<rdc::state::snapshot_store::SnapshotStateStore>>>>,
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
            stores: Arc::default(),
        }
    }

    /// The same project over another lock manager.
    pub fn with_locks(mut self, locks: Arc<dyn EnvLocks>) -> Self {
        self.locks = locks;
        self
    }

    /// Keep each env's state (`.rdc/state/<env>.*`, `.rdc/conflicts/<env>/`,
    /// `envs/<env>/`) in the host's `backend` from the next `sync`, `watch`
    /// or `migrate` on.
    pub fn use_state_store(&self, backend: Arc<dyn SnapshotBackend>) {
        *self.state.lock().unwrap() = Some(backend);
        self.stores.lock().unwrap().clear();
    }

    fn state_on(&self) -> bool {
        self.state.lock().unwrap().is_some()
    }

    /// Where a run keeps `env`'s state: the host's store when one is set,
    /// else the tree itself.
    fn state_store(&self, env: &str) -> Arc<dyn StateStore> {
        let Some(backend) = self.state.lock().unwrap().clone() else {
            return Arc::new(FsStateStore);
        };
        self.stores
            .lock()
            .unwrap()
            .entry(env.to_string())
            .or_insert_with(|| Arc::new(rdc::state::snapshot_store::SnapshotStateStore::new(backend)))
            .clone()
    }

    /// A renamed or removed env's store remembers a tree that is gone.
    fn forget_store(&self, env: &str) {
        self.stores.lock().unwrap().remove(env);
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
        self.forget_store(env);
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
        if new.trim() != old {
            self.forget_store(old);
        }
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
        let _lock = self.lock_env(&env, &api_base, org_id).await?;
        let root = self.root.clone();
        let store = self.store();
        let o = rdc::vfs::scope(store.clone(), async {
            rdc::cli::init::write_scaffold_files(&root, &env, &api_base, org_id)?;
            let token = rdc::secrets::resolve_token(&root, &env, &api_base).await?;
            let state = self.state_store(&env);
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
            plan: o.plan.map(Into::into),
            skipped: o.skipped.into_iter().map(Into::into).collect(),
        })
    }

    /// `rdc migrate <src> <tgt>` on the tree (`cli::migrate::run_at`), its
    /// log on `log`.
    ///
    /// Both envs are claimed for the run. With a host state store set, each
    /// env's state is loaded from it first, the target org's Web Lock is
    /// held throughout, and the target's state is saved after the run (not
    /// under `dryRun`).
    pub async fn migrate(
        self,
        src: String,
        tgt: String,
        opts: MigrateOptions,
        log: Box<dyn Write + Send>,
    ) -> Result<MigrateResult> {
        let opts = embed::EmbedMigrateOptions::try_from(opts)?;
        // The same env twice is `run_at`'s error to give, not a busy env.
        let _claims = if src == tgt { None } else { Some((self.claim(&src)?, self.claim(&tgt)?)) };
        let root = self.root.clone();
        let store = self.store();
        let mut lock = None;
        let (src_state, tgt_state): (Arc<dyn StateStore>, Arc<dyn StateStore>) = if self.state_on() && src != tgt {
            let (tgt_base, tgt_org) = self.env_config(&tgt)?;
            lock = Some(self.lock_env(&tgt, &tgt_base, tgt_org).await?);
            (self.state_store(&src), self.state_store(&tgt))
        } else {
            (Arc::new(FsStateStore), Arc::new(FsStateStore))
        };
        let report = embed::migrate_logged_in(store, src_state, tgt_state, &root, &src, &tgt, opts, log).await?;
        drop(lock);
        Ok(MigrateResult::from_report(&src, &tgt, report))
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
            let _lock = this.lock_env(&env, &api_base, org_id).await?;
            let root = this.root.clone();
            let store = this.store();
            let token = rdc::vfs::scope(store.clone(), async {
                rdc::cli::init::write_scaffold_files(&root, &env, &api_base, org_id)?;
                rdc::secrets::resolve_token(&root, &env, &api_base).await
            })
            .await?;
            let state = this.state_store(&env);
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
    fn a_recovery_prompt_keeps_its_whole_question() {
        let prompt = embed::Prompt {
            kind: embed::PromptKind::RecoverCreate,
            question: "labels/a: an unfinished run's create may have made any of 2 labels (ids 7, 9). Which one did it create?".into(),
            keys: vec![embed::PromptKey::new('1', "it is id 7"), embed::PromptKey::new('2', "it is id 9"), embed::PromptKey::new('a', "abort the sync")],
        };
        let (dto, allowed) = offer(&prompt);
        assert_eq!(dto.kind, PromptKind::RecoverCreate);
        assert_eq!(dto.question, prompt.question);
        assert_eq!(dto.choices.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec!["1", "2", "a"]);
        assert_eq!(accept(Some("2".into()), &allowed), Some("2".into()));
        assert_eq!(serde_json::to_value(PromptKind::RecoverCreate).unwrap(), "recoverCreate");
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


    /// `m`, only for requests carrying `token` when one is given (the API
    /// sends `token <t>`).
    fn with_token(m: MockBuilder, token: Option<&str>, scheme: &str) -> MockBuilder {
        match token {
            Some(t) => m.and(header("authorization", format!("{scheme} {t}").as_str())),
            None => m,
        }
    }

    /// An empty org (Data Storage unmounted: no MDH), answering only `token`
    /// when one is given.
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




    /// `acme` with envs `dev` and `prod`, both on `server`'s org 1; `dev`
    /// holds two labels and `prod` one of its own. Both are read once.
    async fn two_envs(server: &MockServer, locks: &MemLocks) -> ProjectCore {
        let p = project_on(server, locks);
        p.add_env(&EnvInput { name: "prod".into(), ..token_env(server) }).unwrap();
        sync(&p, "dev").await.expect("reading dev");
        sync(&p, "prod").await.expect("reading prod");
        let org = format!("{}/organizations/1", server.uri());
        for (key, name) in [("envs/dev/labels/a.json", "a"), ("envs/dev/labels/b.json", "b"), ("envs/prod/labels/c.json", "c")] {
            let body = serde_json::json!({ "name": name, "organization": org });
            p.write_file(key, format!("{}\n", serde_json::to_string_pretty(&body).unwrap()).as_bytes()).unwrap();
        }
        p
    }

    async fn migrate(p: &ProjectCore, opts: MigrateOptions) -> (Result<MigrateResult>, Vec<String>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        let (log, _route) = log_and_route(move |l| sink.lock().unwrap().push(l.to_string()), Box::new(|_| Box::pin(async { None })), None);
        let out = p.clone().migrate("dev".into(), "prod".into(), opts, log).await;
        let lines = lines.lock().unwrap().clone();
        (out, lines)
    }

    #[tokio::test]
    async fn migrate_mirrors_and_reports_the_plan_on_its_log() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let p = two_envs(&server, &MemLocks::default()).await;

        let (out, lines) = migrate(&p, MigrateOptions { mirror: true, ..MigrateOptions::default() }).await;
        let r = out.expect("the migrate");
        assert!(!r.dry_run);
        assert_eq!((r.source.as_str(), r.target.as_str()), ("dev", "prod"));
        let objects: Vec<(&str, &str, ObjectChange)> =
            r.objects.iter().map(|o| (o.kind.as_str(), o.slug.as_str(), o.change)).collect();
        assert_eq!(
            objects,
            vec![("labels", "a", ObjectChange::Create), ("labels", "b", ObjectChange::Create), ("labels", "c", ObjectChange::Delete)]
        );
        assert_eq!((r.creates, r.updates, r.deletes), (2, 0, 1));
        assert_eq!(
            serde_json::to_value(&r.pruned).unwrap(),
            serde_json::json!([{ "target": "envs/prod/labels/c.json", "kind": "labels", "slug": "c" }])
        );
        assert!(r.files.iter().any(|f| f.source == "envs/dev/labels/a.json"
            && f.target == "envs/prod/labels/a.json"
            && f.change == FileChange::Create));
        let a = serde_json::to_value(r.files.iter().find(|f| f.target == "envs/prod/labels/a.json").unwrap()).unwrap();
        assert_eq!((&a["kind"], &a["slug"]), (&serde_json::json!("labels"), &serde_json::json!("a")));
        let files = p.files();
        assert!(files.contains_key("envs/prod/labels/a.json") && !files.contains_key("envs/prod/labels/c.json"));
        assert!(lines.iter().any(|l| l.contains("migrated envs/dev -> envs/prod")), "{lines:?}");
        assert!(!lines.iter().any(|l| l.is_empty()));
        // The claims are released: the envs sync again.
        assert!(p.claim("dev").is_ok() && p.claim("prod").is_ok());
    }



    #[tokio::test]
    async fn migrate_takes_the_targets_lock_and_the_envs_claims() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let locks = MemLocks::default();
        let p = two_envs(&server, &locks).await;

        let claim = p.claim("dev").unwrap();
        let err = migrate(&p, MigrateOptions::default()).await.0.expect_err("dev is busy");
        assert!(err.to_string().contains("\"dev\" is already being synced or watched"), "{err}");
        drop(claim);

        p.use_state_store(MemBackend::new());
        let held = locks.acquire(lock_of(&server)).await.unwrap().expect("free");
        let err = migrate(&p, MigrateOptions::default()).await.0.expect_err("the org is locked elsewhere");
        assert_eq!(err.to_string(), "env 'prod' is already being synced in another tab");
        drop(held);
        migrate(&p, MigrateOptions::default()).await.0.expect("free again");
        assert!(locks.acquire(lock_of(&server)).await.unwrap().is_some(), "released after the run");
    }

    #[tokio::test]
    async fn migrate_only_promotes_the_selection() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let p = two_envs(&server, &MemLocks::default()).await;
        let r = migrate(&p, MigrateOptions { only: vec!["labels/b".into()], ..MigrateOptions::default() }).await.0.unwrap();
        assert_eq!(r.objects.iter().map(|o| o.slug.as_str()).collect::<Vec<_>>(), vec!["b"]);
        assert!(!p.files().contains_key("envs/prod/labels/a.json"));
    }



    #[tokio::test]
    async fn a_dry_run_sync_returns_its_plan_as_camel_case() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let p = two_envs(&server, &MemLocks::default()).await;
        let opts = SyncOptions { interactive: false, dry_run: true, ..SyncOptions::default() };
        let o = p.clone().sync("dev".into(), opts, Box::new(std::io::sink()), quiet_route()).await.unwrap();
        let v = serde_json::to_value(&o).unwrap();
        let items = v["plan"]["items"].as_array().expect("a plan");
        assert!(
            items.contains(&serde_json::json!({
                "kind": "labels", "slug": "a", "direction": "push", "change": "create", "prompt": null, "note": null
            })),
            "{v}"
        );
        assert_eq!(v["plan"]["recovered"], serde_json::json!([]));
        assert_eq!(v["skipped"], serde_json::json!([]));
        let plain = p.clone().sync("dev".into(), SyncOptions { interactive: false, no_push: true, ..SyncOptions::default() },
            Box::new(std::io::sink()), quiet_route()).await.unwrap();
        assert!(plain.plan.is_none());
        let names: Vec<String> = [
            rdc::cli::sync::plan::SkipReason::DeleteGateDeclined,
            rdc::cli::sync::plan::SkipReason::MdhRowDeleteDeclined,
        ]
        .into_iter()
        .map(|r| {
            let item: SkippedItem = rdc::cli::sync::plan::SkippedItem { kind: "k".into(), slug: "s".into(), reason: r }.into();
            serde_json::to_value(item).unwrap()["reason"].as_str().unwrap().to_string()
        })
        .collect();
        assert_eq!(names, vec!["deleteGateDeclined", "mdhRowDeleteDeclined"]);
    }

    #[test]
    fn migrate_options_map_onto_the_cli_flags() {
        let o: MigrateOptions = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(o, MigrateOptions::default());
        let o: MigrateOptions = serde_json::from_value(serde_json::json!({
            "dryRun": true, "mirror": true, "allowRecreate": true, "only": ["hooks/*"], "carry": ["emailPrefixes", "automation"]
        }))
        .unwrap();
        let e = embed::EmbedMigrateOptions::try_from(o).unwrap();
        assert_eq!(e.mirror, embed::MirrorMode::Mirror { allow_recreate: true });
        assert!(e.dry_run);
        assert_eq!(e.only, vec!["hooks/*"]);
        assert_eq!(e.carry, embed::Carry { score_thresholds: false, email_prefixes: true, automation: true });
        let all: MigrateOptions = serde_json::from_value(serde_json::json!({ "carry": ["all"] })).unwrap();
        assert_eq!(
            embed::EmbedMigrateOptions::try_from(all).unwrap().carry,
            embed::Carry { score_thresholds: true, email_prefixes: true, automation: true }
        );

        let lone = MigrateOptions { allow_recreate: true, ..MigrateOptions::default() };
        assert_eq!(embed::EmbedMigrateOptions::try_from(lone).unwrap_err().to_string(), "migrate: `allowRecreate` needs `mirror`.");
        assert!(serde_json::from_value::<MigrateOptions>(serde_json::json!({ "carry": ["thresholds"] })).is_err());
        assert!(serde_json::from_value::<MigrateOptions>(serde_json::json!({ "dry_run": true })).is_err());
        let keys = |ks: &[&str]| check_migrate_option_keys(ks.iter().map(|k| k.to_string()));
        assert!(keys(&["dryRun", "mirror", "allowRecreate", "only", "carry"]).is_ok());
        assert_eq!(keys(&["mirror", "prune"]).unwrap_err().to_string(), "unknown option 'prune'");
    }





    #[tokio::test]
    async fn default_stays_in_memory() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let p = project_on(&server, &MemLocks::default());

        sync(&p, "dev").await.expect("the sync succeeds");

        assert!(p.files().contains_key(".rdc/state/dev.lock.json"), "the state stays in the tree");
    }

    #[tokio::test]
    async fn a_lock_held_in_another_tab_refuses_the_sync_and_the_watch() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let locks = MemLocks::default();
        let p = project_on(&server, &locks);
        p.use_state_store(MemBackend::new());
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
    async fn a_stateful_tab_and_a_plain_tab_on_one_org_exclude_each_other() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let locks = MemLocks::default();
        let with_state = project_on(&server, &locks);
        with_state.use_state_store(MemBackend::new());
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
    async fn a_watch_with_a_host_state_store_survives_a_401() {
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
        let backend = MemBackend::new();
        p.use_state_store(backend.clone());

        let (control, fut, mut lines, mut idles) = start_watch(&p);
        let drive = async {
            started(&mut lines).await;
            // `t1` is revoked: it now gets 401 everywhere; a re-login hands out `t2`.
            server.reset().await;
            for revoked in ["token t1"] {
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
        assert!(relogin > 0);
        assert!(backend.loads() >= 3, "every cycle loaded the state: {}", backend.loads());
    }

    /// A JS-shaped host store: `load`/`save` over memory, with the stored rev
    /// checked atomically, and every call counted.
    #[derive(Default)]
    struct MemBackend {
        stored: Mutex<BTreeMap<String, (u64, Vec<u8>)>>,
        calls: Mutex<Vec<(String, String)>>,
        /// When set, every save answers this error.
        fail_saves: Mutex<Option<String>>,
        /// When set, another tab saves just before the next save.
        race_next_save: Mutex<bool>,
    }

    impl MemBackend {
        fn new() -> Arc<MemBackend> {
            Arc::new(MemBackend::default())
        }
        fn count(&self, what: &str, env: &str) -> usize {
            self.calls.lock().unwrap().iter().filter(|(w, e)| w == what && e == env).count()
        }
        fn loads(&self) -> usize {
            self.calls.lock().unwrap().iter().filter(|(w, _)| w == "load").count()
        }
        fn files(&self, env: &str) -> BTreeMap<String, String> {
            let stored = self.stored.lock().unwrap();
            rdc::state::snapshot::decode(&stored[env].1).unwrap()
        }
    }

    impl SnapshotBackend for MemBackend {
        fn load<'a>(&'a self, env: &'a str) -> rdc::rt::BoxFuture<'a, Result<rdc::state::snapshot_store::Stored>> {
            self.calls.lock().unwrap().push(("load".into(), env.into()));
            let got = self.stored.lock().unwrap().get(env).cloned();
            Box::pin(async move { Ok(got) })
        }
        fn save<'a>(
            &'a self,
            env: &'a str,
            snapshot: Vec<u8>,
            expected_rev: Option<u64>,
            next_rev: u64,
        ) -> rdc::rt::BoxFuture<'a, Result<bool>> {
            self.calls.lock().unwrap().push(("save".into(), env.into()));
            let out = match self.fail_saves.lock().unwrap().clone() {
                Some(msg) => Err(anyhow!(msg)),
                None => {
                    let mut stored = self.stored.lock().unwrap();
                    if std::mem::take(&mut *self.race_next_save.lock().unwrap())
                        && let Some(e) = stored.get_mut(env)
                    {
                        e.0 += 1;
                    }
                    if stored.get(env).map(|e| e.0) == expected_rev {
                        assert_eq!(next_rev, expected_rev.map_or(1, |r| r + 1), "nextRev is expectedRev + 1");
                        stored.insert(env.to_string(), (next_rev, snapshot));
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                }
            };
            Box::pin(async move { out })
        }
    }

    #[tokio::test]
    async fn a_sync_loads_once_and_saves_once_per_save_point() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let p = project_on(&server, &MemLocks::default());
        let backend = MemBackend::new();
        p.use_state_store(backend.clone());

        sync(&p, "dev").await.expect("the sync succeeds");
        assert_eq!(backend.count("load", "dev"), 1, "one load per run");
        let first_saves = backend.count("save", "dev");
        assert!((1..=3).contains(&first_saves), "a save at the run's start, commit and after _index.md: {first_saves}");
        let files = backend.files("dev");
        assert!(files.contains_key(".rdc/state/dev.lock.json") && files.contains_key("envs/dev/_index.md"), "{:?}", files.keys());

        // A run that changes nothing loads once and saves nothing.
        sync(&p, "dev").await.expect("the second sync");
        assert_eq!((backend.count("load", "dev"), backend.count("save", "dev")), (2, first_saves));
    }

    /// The project keeps one store per env, so its second sync knows the
    /// snapshot its first one saved and keeps the host's edit made between
    /// them. A store per call would see a "first" hydrate and put the saved
    /// tree back over the edit.
    #[tokio::test]
    async fn a_projects_second_sync_keeps_the_hosts_edit() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let backend = MemBackend::new();
        let p = project_on(&server, &MemLocks::default());
        p.use_state_store(backend.clone());
        sync(&p, "dev").await.expect("loading dev");
        p.write_file("envs/dev/overlay.toml", b"version = 1\n# the host's edit\n").unwrap();
        p.write_file("envs/dev/labels/new.json", b"{}\n").unwrap();
        sync(&p, "dev").await.expect("the second sync");
        let files = p.files();
        assert_eq!(files["envs/dev/overlay.toml"], b"version = 1\n# the host's edit\n");
        assert!(files.contains_key("envs/dev/labels/new.json"));

        // A new project (a reopened tab) gets the edit back from the store.
        let reopened = ProjectCore::open(files.into_iter().filter(|(k, _)| k == "rdc.toml" || k.starts_with("secrets/")))
            .unwrap()
            .with_locks(Arc::new(MemLocks::default()));
        reopened.use_state_store(backend.clone());
        sync(&reopened, "dev").await.expect("the reopened tab's sync");
        assert_eq!(reopened.files()["envs/dev/overlay.toml"], b"version = 1\n# the host's edit\n");
    }

    #[tokio::test]
    async fn a_save_the_host_refuses_is_another_runs_change() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let backend = MemBackend::new();
        let p = project_on(&server, &MemLocks::default());
        p.use_state_store(backend.clone());
        sync(&p, "dev").await.unwrap();
        *backend.race_next_save.lock().unwrap() = true;
        p.write_file("envs/dev/labels/new.json", b"{}\n").unwrap();
        let out = sync(&p, "dev").await;
        let err = out.expect_err("another run saved in between");
        assert!(format!("{err:#}").contains("the state of env 'dev' was changed by another run"), "{err:#}");

        *backend.fail_saves.lock().unwrap() = Some("quota exceeded".into());
        p.write_file("envs/dev/labels/other.json", b"{}\n").unwrap();
        let err = sync(&p, "dev").await.expect_err("a failed save fails the run");
        assert!(format!("{err:#}").contains("quota exceeded"), "the host's message: {err:#}");
    }

    #[tokio::test]
    async fn migrate_loads_both_envs_and_saves_the_target_once() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let p = two_envs(&server, &MemLocks::default()).await;
        let backend = MemBackend::new();
        p.use_state_store(backend.clone());

        migrate(&p, MigrateOptions { dry_run: true, ..MigrateOptions::default() }).await.0.unwrap();
        assert_eq!((backend.count("load", "dev"), backend.count("load", "prod")), (1, 1), "one load per env");
        assert_eq!(backend.calls.lock().unwrap().iter().filter(|(w, _)| w == "save").count(), 0, "a dry run saves nothing");

        migrate(&p, MigrateOptions::default()).await.0.unwrap();
        assert_eq!((backend.count("save", "dev"), backend.count("save", "prod")), (0, 1), "one save, of the target");
        assert!(backend.files("prod").contains_key("envs/prod/labels/a.json"));
    }

    /// A dry-run migrate saves nothing, but in a new project its loading of
    /// the envs is a first hydrate: the tree's `envs/<env>/` becomes the
    /// saved one, as for any first run.
    #[tokio::test]
    async fn a_dry_run_migrate_in_a_new_project_loads_the_saved_trees() {
        let server = MockServer::start().await;
        mount_world(&server, None).await;
        let backend = MemBackend::new();
        let first = two_envs(&server, &MemLocks::default()).await;
        first.use_state_store(backend.clone());
        sync(&first, "dev").await.unwrap();
        sync(&first, "prod").await.unwrap();
        let saves = backend.calls.lock().unwrap().iter().filter(|(w, _)| w == "save").count();

        let mut files = first.files();
        files.insert("envs/prod/labels/stray.json".into(), b"{}\n".to_vec());
        files.insert("envs/prod/labels/c.json".into(), b"{\"name\": \"edited before loading\"}\n".to_vec());
        let reopened = ProjectCore::open(files).unwrap().with_locks(Arc::new(MemLocks::default()));
        reopened.use_state_store(backend.clone());
        let r = migrate(&reopened, MigrateOptions { dry_run: true, ..MigrateOptions::default() }).await.0.unwrap();
        assert!(r.dry_run);
        let after = reopened.files();
        assert!(!after.contains_key("envs/prod/labels/stray.json"), "a file the saved tree lacks is removed");
        assert_eq!(after["envs/prod/labels/c.json"], first.files()["envs/prod/labels/c.json"], "the saved version is back");
        assert_eq!(backend.calls.lock().unwrap().iter().filter(|(w, _)| w == "save").count(), saves, "nothing was saved");
    }

    #[test]
    fn the_state_store_must_have_exactly_load_and_save() {
        let check = |ks: &[(&str, bool)]| check_state_store_keys(ks.iter().map(|(k, f)| (k.to_string(), *f)));
        assert!(check(&[("load", true), ("save", true)]).is_ok());
        assert_eq!(check(&[("load", true)]).unwrap_err().to_string(), "useStateStore: `save` is missing");
        assert_eq!(check(&[("load", true), ("save", false)]).unwrap_err().to_string(), "useStateStore: `save` must be a function");
        assert_eq!(
            check(&[("load", true), ("save", true), ("key", false)]).unwrap_err().to_string(),
            "useStateStore: unknown member 'key'"
        );
    }

    /// Two envs of one project, each on its own org (so each its own Web
    /// Lock), synced at once. Both follow a hand rename and so both record a
    /// row in the one project-level `.rdc/mapping.toml`; neither row is lost.
    #[tokio::test]
    async fn concurrent_syncs_of_two_envs_keep_both_mapping_rows() {
        let (a, b) = (MockServer::start().await, MockServer::start().await);
        mount_world(&a, None).await;
        mount_world(&b, None).await;
        let p = project_on(&a, &MemLocks::default());
        p.add_env(&EnvInput { name: "prod".into(), ..token_env(&b) }).unwrap();
        // Both envs hold labels `one` and `two`; dev renames `one` by hand,
        // prod renames `two`.
        for (env, server, renamed) in [("dev", &a, "one"), ("prod", &b, "two")] {
            p.run(|root| {
                let paths = rdc::paths::Paths::for_env(root, env);
                let mut lf = rdc::state::Lockfile::default();
                for (slug, id) in [("one", 11u64), ("two", 12)] {
                    let entry = rdc::state::ObjectEntry { id, modified_at: None, modified_by: None, content_hash: None, secrets_hash: None };
                    lf.upsert("labels", slug, entry);
                    let at = if slug == renamed { format!("{slug}-renamed") } else { slug.to_string() };
                    let body = serde_json::json!({
                        "id": id, "url": format!("{}/labels/{id}", server.uri()), "name": slug,
                        "organization": format!("{}/organizations/1", server.uri()),
                    });
                    let file = paths.labels_dir().join(format!("{at}.json"));
                    rdc::vfs::create_dir_all(file.parent().unwrap()).unwrap();
                    rdc::vfs::write(&file, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
                }
                rdc::vfs::create_dir_all(paths.lockfile().parent().unwrap()).unwrap();
                lf.save(&paths.lockfile()).unwrap();
            });
        }

        // A third task watches the two runs overlap: both envs claimed at once.
        let overlapped = async {
            for _ in 0..10_000 {
                if p.busy.lock().unwrap().len() == 2 {
                    return true;
                }
                tokio::task::yield_now().await;
            }
            false
        };
        let (dev, prod, both) = tokio::join!(sync(&p, "dev"), sync(&p, "prod"), overlapped);
        dev.expect("the dev sync");
        prod.expect("the prod sync");
        assert!(both, "the two syncs ran at the same time");

        let text = String::from_utf8(p.files()[".rdc/mapping.toml"].clone()).unwrap();
        let mapping = p.run(|root| rdc::mapping::GenericMapping::load(&root.join(".rdc/mapping.toml"))).unwrap();
        assert_eq!(mapping.orient("dev", "prod").labels.get("one-renamed").map(String::as_str), Some("one"), "{text}");
        assert_eq!(mapping.orient("prod", "dev").labels.get("two-renamed").map(String::as_str), Some("two"), "{text}");
    }
}
