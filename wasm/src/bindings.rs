//! The `#[wasm_bindgen]` layer: JS values and callbacks in, [`ProjectCore`]
//! calls out. No logic of its own beyond the conversions.

use crate::{
    AskFn, EditProjectInput, EnvInput, Prompt, ProjectCore, SyncOptions, WatchControl,
    log_and_route,
};
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
  | "mdhIndexDrop" | "mdhRowDelete" | "unknown";
export interface Prompt { kind: PromptKind; question: string; choices: { key: string; label: string }[]; }
export interface CycleOutcome {
  itemsPushed: number; itemsPulled: number; conflicts: number;
  remoteDeletesResolved: number; fileCount: number;
}
"#;

fn to_js<T: Serialize>(v: &T) -> Result<JsValue, JsError> {
    v.serialize(&serde_wasm_bindgen::Serializer::new().serialize_missing_as_null(true))
        .map_err(|e| JsError::new(&e.to_string()))
}

fn from_js<T: serde::de::DeserializeOwned>(v: JsValue) -> Result<T, JsError> {
    serde_wasm_bindgen::from_value(v).map_err(|e| JsError::new(&e.to_string()))
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

    /// One reconciliation cycle of `env`. Rejects when `env` is already
    /// being synced or watched.
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

    /// Watch `env` until `stop()`. Throws when `env` is already being synced
    /// or watched.
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
