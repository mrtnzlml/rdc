//! The `#[wasm_bindgen]` layer: JS values and callbacks in, [`ProjectCore`]
//! calls out. No logic of its own beyond the conversions.

use crate::{
    AskFn, check_migrate_option_keys, check_state_store_keys, EditProjectInput, EnvInput, EnvLocks, HeldLock,
    MigrateOptions, Prompt, ProjectCore, SnapshotBackend, SyncOptions, WatchControl, log_and_route,
};
use anyhow::anyhow;
use serde::Serialize;
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, future_to_promise};

#[cfg(target_feature = "atomics")]
compile_error!("WasmOnly is sound only on single-threaded wasm32; this build enables threads.");

/// Lets a JS value cross rdc's `Send + Sync` callback bounds.
///
/// JS values are `!Send` because they live on the thread that made them.
/// rdc asks for `Send + Sync` because natively its callbacks may run on
/// another thread. wasm32-unknown-unknown without the `atomics` feature has
/// exactly one thread, so no value can ever reach another one: the impls
/// below are sound there, and this module is built nowhere else (it is
/// `#[cfg(target_arch = "wasm32")]` in lib.rs, and the `compile_error!`
/// above refuses a threaded build).
///
/// Use [`WasmOnly::get`], never `.0`, inside a closure: a closure that names
/// `.0` captures only that `!Send` field, not the wrapper.
struct WasmOnly<T>(T);

// SAFETY: see the type's doc — one thread, so nothing is ever shared or sent.
unsafe impl<T> Send for WasmOnly<T> {}
// SAFETY: as above.
unsafe impl<T> Sync for WasmOnly<T> {}

impl<T> WasmOnly<T> {
    fn get(&self) -> &T {
        &self.0
    }
}

#[wasm_bindgen(typescript_custom_section)]
const TS_TYPES: &'static str = r#"
export type AuthKind = "token" | "password";
export interface EnvInput {
  name: string; apiBase: string; orgId: number; authKind: AuthKind;
  token?: string | null; username?: string | null; password?: string | null;
}
export interface EditProjectInput {
  env: string; name: string; apiBase: string; orgId: number; authKind: AuthKind;
  token?: string | null; username?: string | null; password?: string | null;
}
export interface EnvSummary {
  name: string; apiBase: string; orgId: number; authKind: AuthKind;
  lastSyncUnix: number | null; fileCount: number;
}
export interface ProjectSummary {
  id: string; name: string; folder: string; envs: EnvSummary[]; error: string | null;
}
export interface RenameEnvResult { project: ProjectSummary; followUps: string[]; warnings: string[]; }
export interface SyncOptions {
  interactive?: boolean; allowDeletes?: boolean; noPush?: boolean; noPull?: boolean;
  dryRun?: boolean; conflict?: "useRemote" | "keepLocal" | "skip" | null;
}
export type PromptKind =
  | "conflict" | "remoteDelete" | "bulkConfirm" | "deleteGate" | "deleteDrift"
  | "mdhIndexDrop" | "mdhRowDelete" | "recoverCreate" | "unknown";
export interface Prompt { kind: PromptKind; question: string; choices: { key: string; label: string }[]; }
export interface StoredState { rev: number; snapshot: Uint8Array; }
export interface StateStore {
  load(env: string): Promise<StoredState | null>;
  save(env: string, snapshot: Uint8Array, expectedRev: number | null, nextRev: number): Promise<boolean>;
}
export type CarryGroup = "scoreThresholds" | "emailPrefixes" | "automation" | "all";
export interface MigrateOptions {
  dryRun?: boolean; mirror?: boolean; allowRecreate?: boolean; only?: string[]; carry?: CarryGroup[];
}
export type FileChange = "create" | "update" | "unchanged";
export type ObjectChange = "create" | "update" | "unchanged" | "delete";
export interface MigrateFile {
  source: string; target: string; change: FileChange;
  kind: string | null; slug: string | null;
}
export interface MigrateObject { kind: string; slug: string; change: ObjectChange; }
export interface MigratePruned { target: string; kind: string | null; slug: string | null; }
export interface MigrateResult {
  dryRun: boolean; source: string; target: string;
  files: MigrateFile[]; objects: MigrateObject[];
  pruned: MigratePruned[]; prunedDirs: string[]; skipped: string[]; warnings: string[];
  creates: number; updates: number; unchanged: number; deletes: number;
  filesChanged: number; renamed: number;
}
export type PlanDirection = "push" | "pull" | "both";
export type PlanChange = "create" | "update" | "delete" | "conflict";
export type PlanPrompt = "deleteGate" | "conflict" | "remoteDelete";
export interface PlanItem {
  kind: string; slug: string; direction: PlanDirection; change: PlanChange;
  prompt: PlanPrompt | null; note: string | null;
}
export interface PlanRecovery { kind: string; slug: string; id: number | null; }
export interface SyncPlan { items: PlanItem[]; recovered: PlanRecovery[]; problems: string[]; }
export type SkipReason =
  | "deleteGateDeclined" | "deleteDriftSkipped" | "pushDriftSkipped" | "conflictSkipped"
  | "remoteDeleteSkipped" | "mdhIndexDropDeclined" | "mdhRowDeleteDeclined";
export interface SkippedItem { kind: string; slug: string; reason: SkipReason; }
export interface CycleOutcome {
  itemsPushed: number; itemsPulled: number; conflicts: number;
  remoteDeletesResolved: number; fileCount: number;
  plan: SyncPlan | null; skipped: SkippedItem[];
}
"#;

// ---------------------------------------------------------------- Web Locks

// The two members of `LockManager` this needs, bound by hand: web-sys has
// them only behind `--cfg=web_sys_unstable_apis`.
#[wasm_bindgen]
extern "C" {
    type LockManager;
    #[wasm_bindgen(method)]
    fn request(this: &LockManager, name: &str, options: &JsValue, callback: &JsValue) -> js_sys::Promise;
}

/// [`EnvLocks`] over `navigator.locks`: an exclusive lock, requested with
/// `ifAvailable`, so a lock another tab holds is refused, never waited for.
/// The lock is held until the callback's promise settles, which happens
/// when the [`HeldLock`] is dropped, or when the tab goes away.
pub struct WebLocks;

/// Resolves the promise the lock callback returned, which frees the lock.
struct WebHeld(WasmOnly<js_sys::Function>);

impl Drop for WebHeld {
    fn drop(&mut self) {
        let _ = self.0.get().call0(&JsValue::NULL);
    }
}

fn lock_manager() -> Option<LockManager> {
    let navigator = js_sys::Reflect::get(&js_sys::global(), &"navigator".into()).ok()?;
    if !navigator.is_object() {
        return None;
    }
    let locks = js_sys::Reflect::get(&navigator, &"locks".into()).ok()?;
    locks.is_object().then(|| locks.unchecked_into())
}

impl EnvLocks for WebLocks {
    fn acquire(&self, name: String) -> rdc::rt::BoxFuture<'static, anyhow::Result<Option<HeldLock>>> {
        Box::pin(async move {
            let locks = lock_manager().ok_or_else(|| anyhow!("Web Locks are not available in this context"))?;
            let mut release = None;
            let held = js_sys::Promise::new(&mut |resolve, _| release = Some(resolve));
            let mut grant = None;
            let granted = js_sys::Promise::new(&mut |resolve, _| grant = Some(resolve));
            let (release, grant) = (release.expect("called at once"), grant.expect("called at once"));
            // `null` when another holder has the lock: report it and return
            // at once. Otherwise return `held`, which keeps the lock until
            // `release` is called.
            let callback = Closure::once_into_js(move |lock: JsValue| -> JsValue {
                let _ = grant.call1(&JsValue::NULL, &JsValue::from_bool(!lock.is_null()));
                if lock.is_null() { JsValue::UNDEFINED } else { held.into() }
            });
            let options = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&options, &"mode".into(), &"exclusive".into());
            let _ = js_sys::Reflect::set(&options, &"ifAvailable".into(), &JsValue::TRUE);
            let request = locks.request(&name, &options, &callback);
            // The request rejects without calling back (an invalid name, a
            // document without an origin): its error is the answer then.
            let granted = match futures::future::select(JsFuture::from(granted), JsFuture::from(request)).await {
                futures::future::Either::Left((g, _)) => g,
                futures::future::Either::Right((Err(e), _)) => Err(e),
                futures::future::Either::Right((Ok(_), pending)) => pending.await,
            };
            let granted = granted.map_err(|e| anyhow!("taking the lock {name}: {}", js_error(&e)))?;
            Ok(granted.is_truthy().then(|| Box::new(WebHeld(WasmOnly(release))) as HeldLock))
        })
    }
}

/// A thrown JS value as text.
fn js_error(e: &JsValue) -> String {
    e.dyn_ref::<js_sys::Error>().map_or_else(|| format!("{e:?}"), |e| String::from(e.message()))
}

fn to_js<T: Serialize>(v: &T) -> Result<JsValue, JsError> {
    v.serialize(&serde_wasm_bindgen::Serializer::new().serialize_missing_as_null(true))
        .map_err(|e| JsError::new(&e.to_string()))
}

fn from_js<T: serde::de::DeserializeOwned>(v: JsValue) -> Result<T, JsError> {
    serde_wasm_bindgen::from_value(v).map_err(|e| JsError::new(&e.to_string()))
}

/// The host's `StateStore`: its `load` and `save`, called with the object
/// as `this`. A throw or a rejected promise is a failed load or save, with
/// the host's message.
struct JsStateStore {
    this: WasmOnly<JsValue>,
    load: WasmOnly<js_sys::Function>,
    save: WasmOnly<js_sys::Function>,
}

/// The settled value of what a host callback returned (a value or a promise).
async fn settle(called: Result<JsValue, JsValue>, what: &str) -> anyhow::Result<JsValue> {
    let v = called.map_err(|e| anyhow!("{what}: {}", js_error(&e)))?;
    JsFuture::from(js_sys::Promise::resolve(&v)).await.map_err(|e| anyhow!("{what}: {}", js_error(&e)))
}

impl SnapshotBackend for JsStateStore {
    fn load<'a>(&'a self, env: &'a str) -> rdc::rt::BoxFuture<'a, anyhow::Result<rdc::state::snapshot_store::Stored>> {
        let called = self.load.get().call1(self.this.get(), &JsValue::from_str(env));
        Box::pin(async move {
            let v = settle(called, "the state store's load failed").await?;
            if v.is_null() || v.is_undefined() {
                return Ok(None);
            }
            let rev = js_sys::Reflect::get(&v, &"rev".into())
                .ok()
                .and_then(|r| r.as_f64())
                .filter(|r| *r >= 0.0 && r.fract() == 0.0)
                .ok_or_else(|| anyhow!("the state store's load returned no whole `rev`"))?;
            let snapshot = js_sys::Reflect::get(&v, &"snapshot".into())
                .ok()
                .filter(|s| s.is_instance_of::<js_sys::Uint8Array>())
                .ok_or_else(|| anyhow!("the state store's load returned no `snapshot` Uint8Array"))?;
            Ok(Some((rev as u64, js_sys::Uint8Array::new(&snapshot).to_vec())))
        })
    }

    fn save<'a>(
        &'a self,
        env: &'a str,
        snapshot: Vec<u8>,
        expected_rev: Option<u64>,
        next_rev: u64,
    ) -> rdc::rt::BoxFuture<'a, anyhow::Result<bool>> {
        let args = js_sys::Array::of4(
            &JsValue::from_str(env),
            &js_sys::Uint8Array::from(&snapshot[..]),
            &expected_rev.map_or(JsValue::NULL, |r| JsValue::from_f64(r as f64)),
            &JsValue::from_f64(next_rev as f64),
        );
        let called = self.save.get().apply(self.this.get(), &args);
        Box::pin(async move {
            let v = settle(called, "the state store's save failed").await?;
            v.as_bool().ok_or_else(|| anyhow!("the state store's save did not resolve to a boolean"))
        })
    }
}

/// rdc's package version.
#[wasm_bindgen]
pub fn version() -> String {
    rdc::version().unwrap_or_default().to_string()
}

/// An rdc project held in memory.
#[wasm_bindgen]
pub struct Project {
    core: ProjectCore,
}

impl Project {
    fn err(&self, e: anyhow::Error) -> JsError {
        JsError::new(&self.core.message(&e))
    }
}

/// `onPrompt` as rdc's prompt callback. A throw, a rejected promise or a
/// non-string answer all read as no answer.
fn ask_fn(on_prompt: js_sys::Function) -> Box<AskFn> {
    let f = WasmOnly(on_prompt);
    Box::new(move |p: Prompt| {
        let called = to_js(&p).ok().and_then(|v| f.get().call1(&JsValue::NULL, &v).ok());
        Box::pin(async move {
            let v = JsFuture::from(js_sys::Promise::resolve(&called?)).await.ok()?;
            v.as_string()
        })
    })
}

fn log_fn(on_log: js_sys::Function) -> impl Fn(&str) + Send + 'static {
    let f = WasmOnly(on_log);
    move |line: &str| {
        let _ = f.get().call1(&JsValue::NULL, &JsValue::from_str(line));
    }
}

#[wasm_bindgen]
impl Project {
    /// An existing project, from its files keyed relative to its root.
    #[wasm_bindgen(constructor)]
    pub fn new(
        #[wasm_bindgen(unchecked_param_type = "Map<string, Uint8Array>")] files: js_sys::Map,
    ) -> Result<Project, JsError> {
        let mut entries = Vec::new();
        for kv in files.entries() {
            let kv: js_sys::Array = kv.map_err(|e| JsError::new(&format!("{e:?}")))?.into();
            let key = kv.get(0).as_string().ok_or_else(|| JsError::new("a file key is not a string"))?;
            entries.push((key, js_sys::Uint8Array::new(&kv.get(1)).to_vec()));
        }
        let core = ProjectCore::open(entries).map_err(|e| JsError::new(&format!("{e:#}")))?;
        Ok(Project { core })
    }

    /// A new project with one env: `rdc.toml`, its secrets and the scaffold.
    pub fn create(
        name: String,
        #[wasm_bindgen(js_name = firstEnv, unchecked_param_type = "EnvInput")] first_env: JsValue,
    ) -> Result<Project, JsError> {
        let input: EnvInput = from_js(first_env)?;
        let core = ProjectCore::create(&name, &input).map_err(|e| JsError::new(&format!("{e:#}")))?;
        Ok(Project { core })
    }

    #[wasm_bindgen(unchecked_return_type = "Map<string, Uint8Array>")]
    pub fn files(&self) -> js_sys::Map {
        let out = js_sys::Map::new();
        for (k, v) in self.core.files() {
            out.set(&JsValue::from_str(&k), &js_sys::Uint8Array::from(&v[..]));
        }
        out
    }

    /// `cb(path, bytes)` after every write; `bytes` is `null` for a removal.
    #[wasm_bindgen(js_name = onChange)]
    pub fn on_change(
        &self,
        #[wasm_bindgen(unchecked_param_type = "(path: string, bytes: Uint8Array | null) => void")]
        cb: js_sys::Function,
    ) {
        let f = WasmOnly(cb);
        self.core.on_change(Box::new(move |path, bytes| {
            let bytes = bytes.map_or(JsValue::NULL, |b| js_sys::Uint8Array::from(b).into());
            let _ = f.get().call2(&JsValue::NULL, &JsValue::from_str(path), &bytes);
        }));
    }

    #[wasm_bindgen(js_name = writeFile)]
    pub fn write_file(&self, path: String, bytes: Vec<u8>) -> Result<(), JsError> {
        self.core.write_file(&path, &bytes).map_err(|e| self.err(e))
    }

    #[wasm_bindgen(js_name = deleteFile)]
    pub fn delete_file(&self, path: String) -> Result<(), JsError> {
        self.core.delete_file(&path).map_err(|e| self.err(e))
    }

    #[wasm_bindgen(unchecked_return_type = "ProjectSummary")]
    pub fn summary(&self) -> Result<JsValue, JsError> {
        to_js(&self.core.summary().map_err(|e| self.err(e))?)
    }

    #[wasm_bindgen(js_name = editProject, unchecked_return_type = "ProjectSummary")]
    pub fn edit_project(
        &self,
        #[wasm_bindgen(unchecked_param_type = "EditProjectInput")] input: JsValue,
    ) -> Result<JsValue, JsError> {
        let input: EditProjectInput = from_js(input)?;
        to_js(&self.core.edit_project(&input).map_err(|e| self.err(e))?)
    }

    #[wasm_bindgen(js_name = addEnv, unchecked_return_type = "ProjectSummary")]
    pub fn add_env(
        &self,
        #[wasm_bindgen(unchecked_param_type = "EnvInput")] input: JsValue,
    ) -> Result<JsValue, JsError> {
        let input: EnvInput = from_js(input)?;
        to_js(&self.core.add_env(&input).map_err(|e| self.err(e))?)
    }

    /// `null` when `env` was the last one: the tree is then empty.
    #[wasm_bindgen(js_name = removeEnv, unchecked_return_type = "ProjectSummary | null")]
    pub fn remove_env(&self, env: String) -> Result<JsValue, JsError> {
        to_js(&self.core.remove_env(&env).map_err(|e| self.err(e))?)
    }

    #[wasm_bindgen(js_name = renameEnv, unchecked_return_type = "RenameEnvResult")]
    pub fn rename_env(
        &self,
        #[wasm_bindgen(js_name = oldName)] old_name: String,
        #[wasm_bindgen(js_name = newName)] new_name: String,
    ) -> Result<JsValue, JsError> {
        to_js(&self.core.rename_env(&old_name, &new_name).map_err(|e| self.err(e))?)
    }

    /// Keep each env's sync state and tree in the host's `store` from the
    /// next `sync`, `watch` or `migrate` on (see `StateStore`). Throws unless
    /// `store` is an object whose own members are exactly the functions
    /// `load` and `save`.
    #[wasm_bindgen(js_name = useStateStore)]
    pub fn use_state_store(
        &self,
        #[wasm_bindgen(unchecked_param_type = "StateStore")] store: JsValue,
    ) -> Result<(), JsError> {
        if !store.is_object() {
            return Err(self.err(anyhow!("useStateStore: the store must be an object with `load` and `save`")));
        }
        let obj: &js_sys::Object = store.unchecked_ref();
        let keys: Vec<(String, bool)> = js_sys::Object::keys(obj)
            .iter()
            .filter_map(|k| k.as_string())
            .map(|k| {
                let is_fn = js_sys::Reflect::get(obj, &JsValue::from_str(&k)).is_ok_and(|v| v.is_function());
                (k, is_fn)
            })
            .collect();
        check_state_store_keys(keys).map_err(|e| self.err(e))?;
        let member = |k: &str| -> js_sys::Function { js_sys::Reflect::get(obj, &JsValue::from_str(k)).unwrap().unchecked_into() };
        let backend = JsStateStore { this: WasmOnly(store.clone()), load: WasmOnly(member("load")), save: WasmOnly(member("save")) };
        self.core.use_state_store(Arc::new(backend));
        Ok(())
    }

    /// One reconciliation cycle of `env`. Rejects when `env` is already
    /// being synced or watched, here or in another tab (its Web Lock).
    #[wasm_bindgen(unchecked_return_type = "Promise<CycleOutcome>")]
    pub fn sync(
        &self,
        env: String,
        #[wasm_bindgen(unchecked_param_type = "SyncOptions")] opts: JsValue,
        #[wasm_bindgen(js_name = onPrompt, unchecked_param_type = "(p: Prompt) => Promise<string | null>")]
        on_prompt: js_sys::Function,
        #[wasm_bindgen(js_name = onLog, unchecked_param_type = "(line: string) => void")]
        on_log: js_sys::Function,
    ) -> Result<js_sys::Promise, JsError> {
        let opts: SyncOptions =
            if opts.is_undefined() || opts.is_null() { SyncOptions::default() } else { from_js(opts)? };
        let (log, route) = log_and_route(log_fn(on_log), ask_fn(on_prompt), None);
        let core = self.core.clone();
        Ok(future_to_promise(async move {
            match core.clone().sync(env, opts, log, route).await {
                Ok(o) => Ok(to_js(&o)?),
                Err(e) => Err(JsError::new(&core.message(&e)).into()),
            }
        }))
    }

    /// `rdc migrate src tgt` on the tree. Resolves with what it changed (or,
    /// under `dryRun`, would change). Throws on an unknown option or
    /// `allowRecreate` without `mirror`; rejects when either env is busy here
    /// or, with a state store set, the target's org is locked in another
    /// tab.
    #[wasm_bindgen(unchecked_return_type = "Promise<MigrateResult>")]
    pub fn migrate(
        &self,
        src: String,
        tgt: String,
        #[wasm_bindgen(unchecked_param_type = "MigrateOptions | null | undefined")] opts: JsValue,
        #[wasm_bindgen(js_name = onLog, unchecked_param_type = "(line: string) => void")]
        on_log: js_sys::Function,
    ) -> Result<js_sys::Promise, JsError> {
        let opts: MigrateOptions = if opts.is_undefined() || opts.is_null() {
            MigrateOptions::default()
        } else {
            if opts.is_object() {
                let keys: Vec<String> =
                    js_sys::Object::keys(opts.unchecked_ref::<js_sys::Object>()).iter().filter_map(|k| k.as_string()).collect();
                check_migrate_option_keys(keys).map_err(|e| self.err(e))?;
            }
            from_js(opts)?
        };
        // Migrate asks nothing; the route only carries the log's line buffer.
        let (log, _route) = log_and_route(log_fn(on_log), Box::new(|_| Box::pin(async { None })), None);
        let core = self.core.clone();
        Ok(future_to_promise(async move {
            match core.clone().migrate(src, tgt, opts, log).await {
                Ok(r) => Ok(to_js(&r)?),
                Err(e) => Err(JsError::new(&core.message(&e)).into()),
            }
        }))
    }

    /// Watch `env` until `stop()`. Throws when `env` is already being synced
    /// or watched by this project; `done()` rejects when another tab holds
    /// its Web Lock.
    pub fn watch(
        &self,
        env: String,
        #[wasm_bindgen(js_name = pollSecs)] poll_secs: Option<u32>,
        #[wasm_bindgen(js_name = onPrompt, unchecked_param_type = "(p: Prompt) => Promise<string | null>")]
        on_prompt: js_sys::Function,
        #[wasm_bindgen(js_name = onLog, unchecked_param_type = "(line: string) => void")]
        on_log: js_sys::Function,
        #[wasm_bindgen(js_name = onIdle, unchecked_param_type = "(nextSecs: number | null) => void")]
        on_idle: js_sys::Function,
    ) -> Result<WatchHandle, JsError> {
        let cancel = rdc::cli::sync::watch::CancelToken::new();
        let (log, route) = log_and_route(log_fn(on_log), ask_fn(on_prompt), Some(cancel.clone()));
        let idle = WasmOnly(on_idle);
        let on_idle: Arc<dyn Fn(Option<u64>) + Send + Sync> = Arc::new(move |secs: Option<u64>| {
            let v = secs.map_or(JsValue::NULL, |s| JsValue::from_f64(s as f64));
            let _ = idle.get().call1(&JsValue::NULL, &v);
        });
        let poll = poll_secs.map(|s| std::time::Duration::from_secs(s.into()));
        let (control, fut) = self
            .core
            .watch(env, poll, log, route, Some(on_idle), cancel)
            .map_err(|e| self.err(e))?;
        let core = self.core.clone();
        let done = future_to_promise(async move {
            match fut.await {
                Ok(()) => Ok(JsValue::UNDEFINED),
                Err(e) => Err(JsError::new(&core.message(&e)).into()),
            }
        });
        Ok(WatchHandle { control, done })
    }
}

/// A running watch.
#[wasm_bindgen]
pub struct WatchHandle {
    control: WatchControl,
    done: js_sys::Promise,
}

#[wasm_bindgen]
impl WatchHandle {
    /// Run a cycle now, as a local file change would.
    pub fn trigger(&self) {
        self.control.trigger();
    }

    /// Stop the watch. A pending prompt reads as unanswered.
    pub fn stop(&self) {
        self.control.stop();
    }

    /// Settles when the watch has ended: resolves after `stop()`, rejects
    /// on an error.
    #[wasm_bindgen(unchecked_return_type = "Promise<void>")]
    pub fn done(&self) -> js_sys::Promise {
        self.done.clone()
    }
}
