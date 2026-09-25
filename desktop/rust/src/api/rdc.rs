//! FRB bridge from the Flutter desktop app ("Rossum Local") into the rdc core.
//!
//! Exposes projects that group multiple environments (e.g. dev/test/prod),
//! each syncable independently, and re-uses rdc's own file/credential/sync
//! helpers — this crate adds no new credential or sync logic. Only the FFI
//! glue differs (StreamSink progress + anyhow errors).

use crate::discover::{self, AuthKindRaw, Project};
use anyhow::{anyhow, Result};
use rdc::config::{valid_env_name, INVALID_ENV_NAME_MSG};
use crate::frb_generated::StreamSink;
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};

/// Runs once when the Dart side calls `RustLib.init()`.
#[flutter_rust_bridge::frb(init)]
pub fn init_app() {
    flutter_rust_bridge::setup_default_user_utils();
}

// ---------------------------------------------------------------- types

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    Token,
    Password,
}

impl From<AuthKindRaw> for AuthKind {
    fn from(r: AuthKindRaw) -> Self {
        match r {
            AuthKindRaw::Token => AuthKind::Token,
            AuthKindRaw::Password => AuthKind::Password,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnvSummary {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    pub last_sync_unix: Option<i64>,
    pub file_count: u64,
}

impl From<&crate::discover::EnvInfo> for EnvSummary {
    fn from(e: &crate::discover::EnvInfo) -> Self {
        Self {
            name: e.name.clone(),
            api_base: e.api_base.clone(),
            org_id: e.org_id,
            auth_kind: e.auth_kind.into(),
            last_sync_unix: e.last_sync_unix,
            file_count: e.file_count,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectSummary {
    pub id: String,
    pub name: String,
    pub folder: String,
    pub envs: Vec<EnvSummary>,
}

impl From<&Project> for ProjectSummary {
    fn from(p: &Project) -> Self {
        Self {
            id: p.id().to_string(),
            name: p.name().to_string(),
            folder: p.folder.display().to_string(),
            envs: p.envs.iter().map(EnvSummary::from).collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EditConnectionInput {
    /// The connection name; if it changes, the folder is renamed.
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    /// Credentials are optional on edit: leave all blank to keep the existing
    /// ones. Provide new values (matching `auth_kind`) to replace them.
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// One answerable choice, as offered to the UI. `key` is a String rather
/// than a char because FRB has no char; it is always exactly one character.
#[derive(Debug, Clone)]
pub struct PromptChoice {
    pub key: String,
    pub label: String,
}

/// Mirrors `rdc::cli::stdin_coord::PromptKind` across the bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKindDto {
    Conflict,
    RemoteDelete,
    BulkConfirm,
    DeleteGate,
    DeleteDrift,
    MdhIndexDrop,
    MdhRowDelete,
    /// A coordinated read whose site never announced. Should be unreachable;
    /// it exists so that if it ever happens the UI can say so instead of
    /// silently mislabelling the prompt as a conflict. Render it as an
    /// explicit "unrecognised prompt" state, not as a normal dialog.
    Unknown,
}

#[derive(Debug, Clone)]
pub enum SyncPhase {
    Started,
    /// One line of rdc's real, rendered sync log (plain text, no color).
    Log { line: String },
    /// A cycle is blocked waiting for an answer. Reply with `answer_prompt`
    /// using this `id`. The diff/list this refers to has already arrived as
    /// `Log` lines.
    Prompt {
        id: u64,
        kind: PromptKindDto,
        question: String,
        keys: Vec<PromptChoice>,
    },
    /// The prompt with this id no longer needs an answer (the watch stopped,
    /// or the cycle was torn down). Close the dialog.
    PromptResolved { id: u64 },
    /// A watch is between cycles. `next_poll_secs` is None when polling is
    /// disabled. rdc's own countdown never reaches an embedder — its
    /// in-place status line is a no-op off a TTY — so the app draws its own.
    Idle { next_poll_secs: Option<u64> },
    Done { file_count: u64 },
    Error { message: String },
    Stopped,
}

// ---------------------------------------------------------------- version

/// rdc's package version, surfaced to the app's About box.
pub fn rdc_version() -> Option<String> {
    rdc::version().map(|s| s.to_string())
}

// ---------------------------------------------------------------- list

/// List every Project under `parent`. Non-project folders are skipped.
pub fn list_projects(parent: String) -> Vec<ProjectSummary> {
    discover::scan(Path::new(&parent))
        .iter()
        .map(ProjectSummary::from)
        .collect()
}

// ---------------------------------------------------------------- add

/// Create a new Project: write `rdc.toml` + secrets for a first env named by
/// the caller (`first_env.name`) under a unique slug. (Additional envs are
/// added later via `add_env`.)
pub fn add_project(parent: String, project_name: String, first_env: AddEnvInput) -> Result<ProjectSummary> {
    let parent = PathBuf::from(parent);
    let env_name = first_env.name.trim().to_string();
    if !valid_env_name(&env_name) {
        return Err(anyhow!("Environment name may only contain letters, digits, - and _ (and can't be empty)."));
    }
    let used: HashSet<String> = discover::scan(&parent).iter().map(|p| p.name().to_string()).collect();
    let slug = rdc::slug::slugify_unique(&project_name, &used);
    let folder = parent.join(&slug);
    std::fs::create_dir_all(&folder).map_err(|e| anyhow!("creating folder: {e}"))?;
    // credentials first (validates + writes secrets) so a bad credential never leaves a config behind
    write_credentials(&folder, &env_name, first_env.auth_kind,
        first_env.token.as_deref(), first_env.username.as_deref(), first_env.password.as_deref())?;
    let api_base = first_env.api_base.trim_end_matches('/').to_string();
    let rdc_toml = format!("[envs.{env_name}]\napi_base = \"{api_base}\"\norg_id = {}\n", first_env.org_id);
    std::fs::write(folder.join("rdc.toml"), rdc_toml).map_err(|e| anyhow!("writing rdc.toml: {e}"))?;
    discover::find(&parent, &slug).as_ref().map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("Project not found after add"))
}

// ---------------------------------------------------------------- validate

/// Validate that `path` is an rdc project (≥1 env, any names) and return its
/// summary. Does not move, copy, or symlink anything.
pub fn validate_existing_project(path: String) -> Result<ProjectSummary> {
    let source = PathBuf::from(&path);
    if !source.is_dir() {
        return Err(anyhow!("Not a folder: {path}"));
    }
    if !source.join("rdc.toml").exists() {
        return Err(anyhow!(
            "{path} doesn't look like an rdc project (no rdc.toml). Run `rdc init` there first."
        ));
    }
    discover::inspect(&source)
        .as_ref()
        .map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("{path} has no environments defined in rdc.toml."))
}

// ---------------------------------------------------------------- edit

/// Rename the project folder (from `input.name`) if it changed, and rewrite the
/// named `env`'s api_base/org_id through rdc's own config writer. Credentials
/// are only replaced when supplied (blank = keep existing).
pub fn edit_project(
    folder: String,
    env: String,
    input: EditConnectionInput,
) -> Result<ProjectSummary> {
    let mut folder = PathBuf::from(&folder);
    if !folder.join("rdc.toml").exists() {
        return Err(anyhow!("Project not found"));
    }
    let parent = folder
        .parent()
        .ok_or_else(|| anyhow!("Project has no parent folder"))?
        .to_path_buf();
    let current_slug = folder
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let used: HashSet<String> = discover::scan(&parent)
        .iter()
        .map(|p| p.name().to_string())
        .filter(|n| n != &current_slug)
        .collect();
    let desired_slug = rdc::slug::slugify_unique(&input.name, &used);
    if desired_slug != current_slug {
        let new_folder = parent.join(&desired_slug);
        if new_folder.exists() {
            return Err(anyhow!("A project named \"{}\" already exists here.", input.name));
        }
        std::fs::rename(&folder, &new_folder).map_err(|e| anyhow!("renaming the project: {e}"))?;
        folder = new_folder;
    }

    let toml_path = folder.join("rdc.toml");
    let mut cfg = rdc::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    let ec = cfg
        .envs
        .get_mut(&env)
        .ok_or_else(|| anyhow!("This project has no `{env}` environment."))?;
    ec.api_base = input.api_base.trim_end_matches('/').to_string();
    ec.org_id = input.org_id;
    cfg.save(&toml_path).map_err(|e| anyhow!("{e:#}"))?;

    let has_new_credentials = match input.auth_kind {
        AuthKind::Token => input.token.as_deref().is_some_and(|s| !s.is_empty()),
        AuthKind::Password => {
            input.username.as_deref().is_some_and(|s| !s.is_empty())
                || input.password.as_deref().is_some_and(|s| !s.is_empty())
        }
    };
    if has_new_credentials {
        let _ = std::fs::remove_file(folder.join(format!("secrets/{env}.secrets.json")));
        write_credentials(
            &folder,
            &env,
            input.auth_kind,
            input.token.as_deref(),
            input.username.as_deref(),
            input.password.as_deref(),
        )?;
    }

    discover::inspect(&folder)
        .as_ref()
        .map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("Project not found after edit"))
}

// ---------------------------------------------------------------- add/remove env

#[derive(Debug, Clone)]
pub struct AddEnvInput {
    pub name: String,
    pub api_base: String,
    pub org_id: u64,
    pub auth_kind: AuthKind,
    pub token: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Add a new environment to an existing project. Errors if the env already exists.
pub fn add_env(folder: String, input: AddEnvInput) -> Result<ProjectSummary> {
    let folder = PathBuf::from(&folder);
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(anyhow!("Environment name is required."));
    }
    if !valid_env_name(&name) {
        return Err(anyhow!(INVALID_ENV_NAME_MSG));
    }
    let toml_path = folder.join("rdc.toml");
    let mut cfg = rdc::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    if cfg.envs.contains_key(&name) {
        return Err(anyhow!("An environment named \"{name}\" already exists in this project."));
    }
    // Validate + write credentials FIRST: if they're invalid, `rdc.toml` must
    // stay untouched so the caller can fix the input and retry `add_env`
    // without first having to remove a half-registered env.
    write_credentials(
        &folder, &name,
        input.auth_kind, input.token.as_deref(), input.username.as_deref(), input.password.as_deref(),
    )?;
    cfg.envs.insert(
        name.clone(),
        rdc::config::EnvConfig {
            api_base: input.api_base.trim_end_matches('/').to_string(),
            org_id: input.org_id,
        },
    );
    cfg.save(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    discover::inspect(&folder)
        .as_ref()
        .map(ProjectSummary::from)
        .ok_or_else(|| anyhow!("Project not found after add_env"))
}

/// Remove an environment: its `rdc.toml` section, snapshot, secrets, and state.
/// If it was the last env, the whole project is trashed and `Ok(None)` returned.
pub fn remove_env(folder: String, env: String) -> Result<Option<ProjectSummary>> {
    // Defensive: guard against a malformed `rdc.toml` env key (or any other
    // caller mistake) before `env` is ever interpolated into a path below —
    // an env name with `..` or a path separator must never reach
    // `remove_dir_all`/`remove_file`.
    if !valid_env_name(&env) {
        return Err(anyhow!(INVALID_ENV_NAME_MSG));
    }
    let folder = PathBuf::from(&folder);
    let toml_path = folder.join("rdc.toml");
    let mut cfg = rdc::config::ProjectConfig::load(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    if !cfg.envs.contains_key(&env) {
        return Err(anyhow!("This project has no \"{env}\" environment."));
    }
    if cfg.envs.len() == 1 {
        // Last environment — removing it means removing the project.
        trash_project(folder.display().to_string())?;
        return Ok(None);
    }
    cfg.envs.remove(&env);
    cfg.save(&toml_path).map_err(|e| anyhow!("{e:#}"))?;
    // Best-effort cleanup of the env's on-disk artifacts, all routed through
    // rdc's own `Paths` so the layout stays single-sourced (src/paths.rs).
    let paths = rdc::paths::Paths::for_env(&folder, &env);
    let _ = std::fs::remove_dir_all(paths.env_root());
    let _ = std::fs::remove_file(paths.secrets_file());
    let _ = std::fs::remove_file(paths.lockfile());
    let _ = std::fs::remove_file(paths.env_lock());
    let _ = std::fs::remove_dir_all(paths.base_cache_root());
    // `Paths` has no bare "conflicts dir" accessor, only the per-file
    // `conflict_shadow_path`; passing the env root itself as `local_path`
    // makes it strip to an empty relpath, yielding the env's whole
    // `.rdc/conflicts/<env>/` shadow directory without hand-rolling the path.
    let _ = std::fs::remove_dir_all(paths.conflict_shadow_path(&paths.env_root()));
    discover::inspect(&folder)
        .as_ref()
        .map(ProjectSummary::from)
        .map(Some)
        .ok_or_else(|| anyhow!("Project not found after remove_env"))
}

/// What the app shows after a rename.
#[derive(Debug, Clone)]
pub struct RenameEnvResult {
    pub project: ProjectSummary,
    /// GitLab work rdc cannot do (CI variables, environment history).
    pub follow_ups: Vec<String>,
    pub warnings: Vec<String>,
}

/// Rename an environment `old` → `new`, offline. The work is
/// `rdc edit env rename`'s (`rdc::cli::edit::env::rename_env`), so the app
/// and the CLI rename the same paths, refuse the same cases and roll back
/// the same way.
pub fn rename_env(folder: String, old: String, new: String) -> Result<RenameEnvResult> {
    let folder = PathBuf::from(&folder);
    let summary = |f: &Path| {
        discover::inspect(f)
            .as_ref()
            .map(ProjectSummary::from)
            .ok_or_else(|| anyhow!("Project not found after rename_env"))
    };
    // The edit dialog submits the env name even when it is unchanged.
    if valid_env_name(&old) && new.trim() == old {
        return Ok(RenameEnvResult { project: summary(&folder)?, follow_ups: vec![], warnings: vec![] });
    }
    let report = rdc::cli::edit::env::rename_env(&folder, &old, &new, false).map_err(|e| anyhow!("{e:#}"))?;
    Ok(RenameEnvResult { project: summary(&folder)?, follow_ups: report.follow_ups, warnings: report.warnings })
}

// ---------------------------------------------------------------- sync

/// Two-way sync of one environment. Scaffolds init files, resolves the token
/// (silent re-login in password mode), then runs one reconciliation cycle
/// under `EmbedSyncOptions::default()` (pull and push, prompting on a gate
/// rather than bailing). Progress is streamed as `SyncPhase`.
///
/// Returns `Ok(())` even when the sync itself fails — the terminal outcome
/// (success or error) is conveyed to the caller via the `SyncPhase::Done` /
/// `SyncPhase::Error` stream events, not via this function's `Result`.
pub fn sync_env(
    folder: String,
    env: String,
    api_base: String,
    org_id: u64,
    sink: StreamSink<SyncPhase>,
) -> Result<()> {
    let folder_path = PathBuf::from(&folder);
    let _ = sink.add(SyncPhase::Started);

    // Forward rdc's real, rendered log into the stream, line by line. The
    // buffer is shared with `route` below so a blocking prompt can discard
    // a partial (question) line rather than let it corrupt the next one —
    // see `LineBuffer`'s doc.
    let log_buf = LineBuffer::default();
    let forwarder = LineForwarder {
        sink: sink.clone(),
        buf: log_buf.clone(),
    };

    let cancel = rdc::cli::sync::watch::CancelToken::new();
    let (answer_tx, answer_rx) = std::sync::mpsc::channel::<(u64, String)>();
    let generation = crate::watch_registry::next_id();
    // Registered so `answer_prompt` can find this cycle. A watch on the same
    // env would contend for the env lock anyway, so displacing one here is
    // the same rule `watch_env` applies. Torn down below via `remove_if`,
    // which only deletes OUR registration — see its doc for why a plain
    // key-only remove is unsafe (it could delete a registration that has
    // since displaced this one).
    if let Some(previous) = crate::watch_registry::insert(
        &folder,
        &env,
        crate::watch_registry::WatchHandle {
            id: generation,
            cancel: cancel.clone(),
            answers: answer_tx,
        },
    ) {
        previous.cancel.cancel();
    }
    let route: std::sync::Arc<dyn rdc::cli::sync::embed::PromptRoute> =
        std::sync::Arc::new(SinkPromptRoute {
            sink: sink.clone(),
            answers: std::sync::Mutex::new(answer_rx),
            cancel: cancel.clone(),
            log_buf,
        });

    let result: Result<u64> = block_on(async {
        rdc::cli::init::write_scaffold_files(&folder_path, &env, &api_base, org_id)?;
        let token = rdc::secrets::resolve_token(&folder_path, &env, &api_base).await?;
        rdc::cli::sync::embed::with_route(route, async {
            rdc::cli::sync::embed::sync_logged(
                &folder_path,
                &env,
                &token,
                rdc::cli::sync::embed::EmbedSyncOptions::default(),
                Box::new(forwarder),
            )
            .await
        })
        .await?;
        Ok(discover::count_files(&folder_path.join(format!("envs/{env}"))))
    });

    crate::watch_registry::remove_if(&folder, &env, generation);
    match result {
        Ok(file_count) => {
            // run_cycle omits its closing summary when a renderer is supplied,
            // so add our own completion line before the terminal Done phase.
            let _ = sink.add(SyncPhase::Log {
                line: format!("✓ done · {file_count} files"),
            });
            let _ = sink.add(SyncPhase::Done { file_count });
        }
        Err(e) => {
            let _ = sink.add(SyncPhase::Error {
                message: format!("{e:#}"),
            });
        }
    }
    Ok(())
}

/// A buffer shared between a `LineForwarder` and the `SinkPromptRoute` for
/// the same cycle, so a blocking prompt can drop whatever partial (no
/// trailing newline) line the forwarder is mid-buffering.
///
/// Every prompt writes its question with no trailing newline — correct for
/// a terminal, where the answer is typed on that same line — so at the
/// moment `ask()` is about to block, `LineForwarder`'s buffer holds exactly
/// that un-terminated question text. Left alone, it sits there until the
/// NEXT real log line arrives after the prompt is answered, and gets
/// prepended to it: `[k] keep local … [a] abort > 12:01:14 pull
/// queues/invoices`. `ask()` discards it via this shared handle right
/// before emitting `SyncPhase::Prompt` — the dialog already renders
/// `prompt.question` on its own, so the log pane never needed this text.
///
/// `#[frb(ignore)]` because this is an internal implementation detail shared
/// between `LineForwarder` and `SinkPromptRoute`, not part of the bridged
/// surface. Without it codegen bridges the struct as an opaque Dart class
/// with a `default_()` factory — ~500 lines of glue for a byte buffer the
/// UI has no business holding — and its siblings `LineForwarder` and
/// `SinkPromptRoute` are already ignored for the same reason.
#[flutter_rust_bridge::frb(ignore)]
#[derive(Clone, Default)]
struct LineBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl LineBuffer {
    /// Drop whatever partial bytes are currently buffered, without ever
    /// forwarding them as a `SyncPhase::Log` line.
    fn discard(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// A `std::io::Write` that splits rdc's rendered log output into whole lines
/// and forwards each as `SyncPhase::Log`. rdc writes one newline-terminated
/// line per event (plain text under `ColorMode::Plain`, non-TTY) — except a
/// prompt's question, deliberately left unterminated; see `LineBuffer`.
struct LineForwarder<S: PromptSink = StreamSink<SyncPhase>> {
    sink: S,
    buf: LineBuffer,
}

impl<S: PromptSink> std::io::Write for LineForwarder<S> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut buf = self.buf.0.lock().unwrap();
        buf.extend_from_slice(data);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let text = String::from_utf8_lossy(&line)
                .trim_end_matches(['\n', '\r'])
                .to_string();
            if !text.is_empty() {
                let _ = self.sink.emit(SyncPhase::Log { line: text });
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------- watch

/// Where a prompt (and its resolution) get announced. `StreamSink<SyncPhase>`
/// is the production implementation (below); a fake stands in for it in
/// tests, because `StreamSink::deserialize` needs a real Dart message-port
/// handle and so cannot be built inside a plain `#[test]`.
trait PromptSink {
    /// Send one phase. `false` means the UI side is gone (closed stream).
    fn emit(&self, phase: SyncPhase) -> bool;
}

impl PromptSink for StreamSink<SyncPhase> {
    fn emit(&self, phase: SyncPhase) -> bool {
        self.add(phase).is_ok()
    }
}

/// Turns a blocked core prompt into a `SyncPhase::Prompt` on the stream and
/// blocks until the UI answers through `answer_prompt`.
///
/// `[e]` (shells out to $EDITOR) and `[h]` (a stateful per-hunk walk) are
/// stripped from the offered keys: neither has a meaning in a GUI process.
/// `ask` also re-validates every incoming answer against that same filtered
/// set rather than trusting every future caller to only ever send one of
/// the offered keys — an `e`/`h` that did reach the core would spawn
/// `$EDITOR` or enter the hunk walk, both of which wedge this thread.
struct SinkPromptRoute<S: PromptSink = StreamSink<SyncPhase>> {
    sink: S,
    /// Each answer is tagged with the id the UI believes it is answering
    /// (`answer_prompt`'s `prompt_id`) — see `WatchHandle::answers`.
    answers: std::sync::Mutex<std::sync::mpsc::Receiver<(u64, String)>>,
    /// Cloned from the same `WatchHandle` registered for this cycle. Polled
    /// by the wait loop below so a `stop_watch` that fires while this `ask`
    /// is parked can unblock it — nothing else observes this token while a
    /// prompt is in flight.
    cancel: rdc::cli::sync::watch::CancelToken,
    /// Shared with this cycle's `LineForwarder`. See `LineBuffer`'s doc for
    /// why `ask` discards it before blocking.
    log_buf: LineBuffer,
}

impl<S: PromptSink + Send + Sync> rdc::cli::sync::embed::PromptRoute for SinkPromptRoute<S> {
    fn ask(&self, prompt: &rdc::cli::sync::embed::Prompt) -> Option<String> {
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::Duration;

        // Process-global, not per-route: two generations on the same env (a
        // displaced watch still unwinding a blocked `ask`, and the cycle
        // that displaced it) must never hand out the same id, or a stale
        // `PromptResolved` from the displaced generation could clear the
        // live generation's prompt out from under the user.
        let id = crate::watch_registry::next_prompt_id();
        // `[e]` opens $EDITOR and `[h]` walks hunks on a terminal; neither
        // works here, and `ask` rejects them below. The question is rebuilt
        // from what SURVIVES the filter — cloning the terminal's own line
        // would name two choices this surface does not offer, and the dialog
        // renders that line verbatim.
        let offered: Vec<rdc::cli::sync::embed::PromptKey> = prompt
            .keys
            .iter()
            .filter(|k| !matches!(k.key, 'e' | 'h'))
            .cloned()
            .collect();
        let question = rdc::cli::sync::embed::menu_one_line(&offered);
        let keys: Vec<PromptChoice> = offered
            .iter()
            .map(|k| PromptChoice {
                key: k.key.to_string(),
                label: k.label.clone(),
            })
            .collect();
        // Exactly what was offered, as single characters. Empty means free
        // text — `PromptKind::Unknown`'s fallback offers no keys at all —
        // so accept whatever arrives rather than reject every answer
        // forever.
        let allowed: HashSet<char> = keys.iter().filter_map(|k| k.key.chars().next()).collect();

        let rx = self.answers.lock().unwrap();
        // Drop anything queued from a previous prompt so a late answer can
        // never be read as the answer to this one.
        while rx.try_recv().is_ok() {}

        // Discard the question `LineForwarder` is still holding (written
        // with no trailing newline, as a terminal expects) before it can
        // get prepended to whatever log line arrives once this prompt is
        // answered — see `LineBuffer`'s doc. The dialog renders
        // `prompt.question` on its own, so the log pane doesn't need it.
        self.log_buf.discard();

        if !self.sink.emit(SyncPhase::Prompt {
            id,
            kind: kind_to_dto(prompt.kind),
            question,
            keys,
        }) {
            return None; // Dart stream gone: degrade to EOF (skip / N).
        }

        let answer = loop {
            if self.cancel.is_cancelled() {
                break None; // the watch was stopped while this prompt was parked
            }
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok((answered_id, a)) => {
                    if answered_id != id {
                        // Not an answer to the question we are CURRENTLY
                        // asking — e.g. a UI-side race that fired after
                        // this prompt replaced an earlier one. Same policy
                        // as an off-list key below: ignore and keep
                        // waiting, never apply it.
                        continue;
                    }
                    let first = a.trim().chars().next();
                    if allowed.is_empty() || first.is_some_and(|c| allowed.contains(&c)) {
                        break Some(a);
                    }
                    // Not one of the offered keys. Ignore it and keep
                    // waiting for a real one rather than passing it through.
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break None,
            }
        };
        let _ = self.sink.emit(SyncPhase::PromptResolved { id });
        answer
    }
}

/// No catch-all arm on purpose: a future `PromptKind` variant must fail to
/// compile here rather than silently fall through to some existing label —
/// a wrong label on this path can mean a user answers a question they were
/// never actually asked, on a flow that can delete objects from a live org.
fn kind_to_dto(k: rdc::cli::sync::embed::PromptKind) -> PromptKindDto {
    use rdc::cli::sync::embed::PromptKind as K;
    match k {
        K::Conflict => PromptKindDto::Conflict,
        K::RemoteDelete => PromptKindDto::RemoteDelete,
        K::BulkConfirm => PromptKindDto::BulkConfirm,
        K::DeleteGate => PromptKindDto::DeleteGate,
        K::DeleteDrift => PromptKindDto::DeleteDrift,
        K::MdhIndexDrop => PromptKindDto::MdhIndexDrop,
        K::MdhRowDelete => PromptKindDto::MdhRowDelete,
        K::Unknown => PromptKindDto::Unknown,
    }
}

/// Watch one environment: reconcile once, then re-reconcile on a local file
/// change or on the poll timer, until `stop_watch` is called.
///
/// Blocks the calling FRB pool thread for the watch's whole life (the pool
/// is `num_cpus::get()` threads, so a great many concurrent watches would
/// starve other bridge calls). Progress, prompts and the between-cycle
/// countdown all arrive on `sink`.
///
/// Returns `Ok(())` even when the watch fails: the terminal outcome reaches
/// the caller as `SyncPhase::Error` / `SyncPhase::Stopped`, matching
/// `sync_env`'s contract.
pub fn watch_env(
    folder: String,
    env: String,
    api_base: String,
    org_id: u64,
    poll_secs: Option<u64>,
    sink: StreamSink<SyncPhase>,
) -> Result<()> {
    let folder_path = PathBuf::from(&folder);
    let _ = sink.add(SyncPhase::Started);

    let cancel = rdc::cli::sync::watch::CancelToken::new();
    let (answer_tx, answer_rx) = std::sync::mpsc::channel::<(u64, String)>();
    let generation = crate::watch_registry::next_id();
    let handle = crate::watch_registry::WatchHandle {
        id: generation,
        cancel: cancel.clone(),
        answers: answer_tx,
    };
    // One watch per env: a second would fight the first for the env lock.
    if let Some(previous) = crate::watch_registry::insert(&folder, &env, handle.clone()) {
        previous.cancel.cancel();
    }

    // Shared with `forwarder` below so a blocking prompt can discard a
    // partial (question) line rather than let it corrupt the next one —
    // see `LineBuffer`'s doc.
    let log_buf = LineBuffer::default();
    let route: std::sync::Arc<dyn rdc::cli::sync::embed::PromptRoute> =
        std::sync::Arc::new(SinkPromptRoute {
            sink: sink.clone(),
            answers: std::sync::Mutex::new(answer_rx),
            cancel: cancel.clone(),
            log_buf: log_buf.clone(),
        });

    let forwarder = LineForwarder {
        sink: sink.clone(),
        buf: log_buf,
    };
    let poll = poll_secs.map(std::time::Duration::from_secs);

    // rdc's own in-place "next sync in Ns" countdown is a no-op off a TTY,
    // so nothing else tells the app when a watch goes idle or for how long
    // — this is that signal, one `SyncPhase::Idle` per completed cycle.
    let idle_sink = sink.clone();
    let on_idle: std::sync::Arc<dyn Fn(Option<u64>) + Send + Sync> =
        std::sync::Arc::new(move |next_poll_secs| {
            let _ = idle_sink.add(SyncPhase::Idle { next_poll_secs });
        });

    let result: Result<()> = block_on(async {
        rdc::cli::init::write_scaffold_files(&folder_path, &env, &api_base, org_id)?;
        let token = rdc::secrets::resolve_token(&folder_path, &env, &api_base).await?;
        rdc::cli::sync::embed::watch_logged(
            &folder_path,
            &env,
            &api_base,
            token,
            poll,
            Box::new(forwarder),
            route,
            cancel,
            Some(on_idle),
        )
        .await
    });

    crate::watch_registry::remove_if(&folder, &env, generation);
    match result {
        Ok(()) => {
            let _ = sink.add(SyncPhase::Stopped);
        }
        Err(e) => {
            let _ = sink.add(SyncPhase::Error {
                message: format!("{e:#}"),
            });
        }
    }
    Ok(())
}

/// Ask a running watch to stop. Also unblocks a cycle currently parked on a
/// prompt (`SinkPromptRoute::ask` polls this same token): the poll loop
/// reads the cancellation as end-of-input, exactly like a closed UI stream.
/// No-op if that env is not being watched.
pub fn stop_watch(folder: String, env: String) -> Result<()> {
    if let Some(h) = crate::watch_registry::get(&folder, &env) {
        h.cancel.cancel();
    }
    Ok(())
}

/// Answer the prompt a watch (or a one-shot sync) is currently blocked on.
/// `prompt_id` must be the id of the `SyncPhase::Prompt` being answered —
/// `SinkPromptRoute::ask` accepts an answer only when it matches the
/// question it is currently asking, so a mis-delivered or stale answer (a
/// UI race, a leftover call from a prompt that has already moved on) is
/// ignored rather than applied to the wrong question. On a path whose
/// answer can authorise deleting objects from a live organization, that
/// check is load-bearing, not defensive polish.
///
/// No-op if nothing on that env is waiting — an answer for a prompt that
/// has already been torn down is dropped, not queued.
pub fn answer_prompt(folder: String, env: String, prompt_id: u64, answer: String) -> Result<()> {
    if let Some(h) = crate::watch_registry::get(&folder, &env) {
        let _ = h.answers.send((prompt_id, answer));
    }
    Ok(())
}

// ---------------------------------------------------------------- trash / reveal

/// Move a managed Project's folder to the OS trash/recycle bin.
pub fn trash_project(folder: String) -> Result<()> {
    let path = PathBuf::from(&folder);
    #[cfg(target_os = "macos")]
    {
        // Use NSFileManager rather than the default Finder/AppleScript backend,
        // which requires Apple Events ("Automation") permission the app does not
        // have (error -1743). NSFileManager needs no such permission.
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        let mut ctx = trash::TrashContext::default();
        ctx.set_delete_method(DeleteMethod::NsFileManager);
        ctx.delete(&path)
            .map_err(|e| anyhow!("Couldn't move the folder to the Trash: {e}"))?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        trash::delete(&path).map_err(|e| anyhow!("Couldn't move the folder to the Trash: {e}"))?;
    }
    Ok(())
}

/// Reveal a path in the OS file manager (Finder / Explorer / file manager).
pub fn reveal_in_file_manager(path: String) -> Result<()> {
    let p = PathBuf::from(&path);
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&p)
            .spawn()
            .map_err(|e| anyhow!("open -R: {e}"))?;
    }
    #[cfg(target_os = "windows")]
    {
        // explorer returns a non-zero exit even on success, so don't check it.
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", p.display()))
            .spawn()
            .map_err(|e| anyhow!("explorer /select: {e}"))?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let uri = format!("file://{}", p.display());
        let dbus = std::process::Command::new("dbus-send")
            .args([
                "--session",
                "--dest=org.freedesktop.FileManager1",
                "--type=method_call",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
            ])
            .arg(format!("array:string:{uri}"))
            .arg("string:")
            .spawn();
        if dbus.is_err() {
            let dir = p.parent().unwrap_or(&p);
            std::process::Command::new("xdg-open")
                .arg(dir)
                .spawn()
                .map_err(|e| anyhow!("xdg-open: {e}"))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- internal

/// Write credentials via rdc's own helpers. Empty strings are rejected the
/// same way the original bridge did.
fn write_credentials(
    folder: &Path,
    env: &str,
    auth: AuthKind,
    token: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<()> {
    match auth {
        AuthKind::Token => {
            let t = token
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("Token is required."))?;
            rdc::secrets::write_secrets_file(folder, env, t, None).map_err(|e| anyhow!("{e:#}"))?;
        }
        AuthKind::Password => {
            let u = username
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("Username is required."))?;
            let p = password
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("Password is required."))?;
            rdc::secrets::save_password_credentials(folder, env, u, p)
                .map_err(|e| anyhow!("{e:#}"))?;
        }
    }
    Ok(())
}

/// Block the calling thread on `fut` using a current-thread runtime (the sync
/// engine holds `!Send` types across awaits, so it cannot run multi-threaded).
fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread tokio runtime")
        .block_on(fut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdc::cli::sync::embed::PromptRoute as _;

    #[test]
    fn project_summary_from_multi_env_project() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("acme");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("rdc.toml"),
            "[envs.dev]\napi_base = \"https://d.test/api/v1\"\norg_id = 1\n\
             [envs.prod]\napi_base = \"https://p.test/api/v1\"\norg_id = 2\n",
        )
        .unwrap();
        let p = discover::inspect(&folder).unwrap();
        let summary = ProjectSummary::from(&p);
        assert_eq!(summary.name, "acme");
        assert_eq!(summary.envs.len(), 2);
        assert_eq!(summary.envs[0].name, "dev");
        assert_eq!(summary.envs[1].org_id, 2);
    }

    fn seed_project(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let folder = dir.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("rdc.toml"),
            "[envs.main]\napi_base = \"https://m.test/api/v1\"\norg_id = 1\n").unwrap();
        rdc::secrets::write_secrets_file(&folder, "main", "tok", None).unwrap();
        folder
    }

    #[test]
    fn add_env_appends_a_second_env() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let p = add_env(folder.display().to_string(), AddEnvInput {
            name: "prod".into(), api_base: "https://p.test/api/v1/".into(), org_id: 2,
            auth_kind: AuthKind::Token, token: Some("tok2".into()), username: None, password: None,
        }).unwrap();
        assert_eq!(p.envs.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), vec!["main", "prod"]);
        let prod = p.envs.iter().find(|e| e.name == "prod").unwrap();
        assert_eq!(prod.api_base, "https://p.test/api/v1"); // trailing slash trimmed
        assert!(folder.join("secrets/prod.secrets.json").exists());
    }

    #[test]
    fn add_env_rejects_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let err = add_env(folder.display().to_string(), AddEnvInput {
            name: "main".into(), api_base: "https://x.test/api/v1".into(), org_id: 9,
            auth_kind: AuthKind::Token, token: Some("t".into()), username: None, password: None,
        }).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"));
    }

    #[test]
    fn add_env_rejects_invalid_name() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        for bad in ["../x", "a/b", "..", ""] {
            let err = add_env(folder.display().to_string(), AddEnvInput {
                name: bad.into(), api_base: "https://x.test/api/v1".into(), org_id: 9,
                auth_kind: AuthKind::Token, token: Some("t".into()), username: None, password: None,
            }).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("required") || msg.contains("letters, digits"),
                "unexpected error for {bad:?}: {msg}"
            );
        }
        // Nothing outside the project folder was touched, and rdc.toml still
        // has only the original env.
        let cfg = rdc::config::ProjectConfig::load(&folder.join("rdc.toml")).unwrap();
        assert_eq!(cfg.envs.keys().collect::<Vec<_>>(), vec!["main"]);
    }

    #[test]
    fn remove_env_rejects_invalid_name() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let err = remove_env(folder.display().to_string(), "../../etc".into()).unwrap_err();
        assert!(format!("{err:#}").contains("letters, digits"));
        // The (legitimate) project is entirely untouched.
        assert!(folder.join("rdc.toml").exists());
    }

    #[test]
    fn remove_env_drops_one_and_keeps_project() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        add_env(folder.display().to_string(), AddEnvInput {
            name: "prod".into(), api_base: "https://p.test/api/v1".into(), org_id: 2,
            auth_kind: AuthKind::Token, token: Some("t2".into()), username: None, password: None,
        }).unwrap();
        // Seed on-disk artifacts the cleanup is supposed to remove, so the
        // cleanup lines are actually exercised (not just the rdc.toml edit).
        let paths = rdc::paths::Paths::for_env(&folder, "prod");
        std::fs::create_dir_all(paths.env_root()).unwrap();
        std::fs::write(paths.env_root().join("organization.json"), "{}").unwrap();
        std::fs::create_dir_all(paths.lockfile().parent().unwrap()).unwrap();
        std::fs::write(paths.lockfile(), "{}").unwrap();
        std::fs::write(paths.env_lock(), "").unwrap();
        std::fs::create_dir_all(paths.base_cache_root()).unwrap();
        std::fs::write(paths.base_cache_root().join("organization.json"), "{}").unwrap();
        let conflicts_dir = folder.join(".rdc/conflicts/prod");
        std::fs::create_dir_all(&conflicts_dir).unwrap();
        std::fs::write(conflicts_dir.join("stray.json"), "{}").unwrap();

        let p = remove_env(folder.display().to_string(), "prod".into()).unwrap().unwrap();
        assert_eq!(p.envs.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), vec!["main"]);
        assert!(!folder.join("secrets/prod.secrets.json").exists());
        assert!(!paths.env_root().exists(), "envs/prod should be removed");
        assert!(!paths.lockfile().exists(), "state lockfile.json should be removed");
        assert!(!paths.env_lock().exists(), "state .lock should be removed");
        assert!(!paths.base_cache_root().exists(), "base cache should be removed");
        assert!(!conflicts_dir.exists(), "conflicts shadow dir should be removed");
    }

    #[test]
    fn remove_last_env_trashes_project_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let r = remove_env(folder.display().to_string(), "main".into()).unwrap();
        assert!(r.is_none());
        assert!(!folder.exists()); // whole project gone (moved to trash)
    }

    #[test]
    fn add_project_uses_the_given_first_env_name() {
        let tmp = tempfile::tempdir().unwrap();
        let p = add_project(
            tmp.path().display().to_string(),
            "Acme".into(),
            AddEnvInput { name: "prod".into(), api_base: "https://p.test/api/v1/".into(), org_id: 7,
                auth_kind: AuthKind::Token, token: Some("t".into()), username: None, password: None },
        ).unwrap();
        assert_eq!(p.envs.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), vec!["prod"]);
        assert_eq!(p.envs[0].api_base, "https://p.test/api/v1"); // trailing slash trimmed
        let folder = tmp.path().join(&p.id);
        assert!(folder.join("secrets/prod.secrets.json").exists());
        assert!(!folder.join("secrets/main.secrets.json").exists());
    }

    #[test]
    fn add_project_rejects_invalid_first_env_name() {
        let tmp = tempfile::tempdir().unwrap();
        let err = add_project(tmp.path().display().to_string(), "Acme".into(),
            AddEnvInput { name: "../x".into(), api_base: "https://p.test/api/v1".into(), org_id: 1,
                auth_kind: AuthKind::Token, token: Some("t".into()), username: None, password: None }).unwrap_err();
        assert!(format!("{err:#}").to_lowercase().contains("environment name"));
    }

    #[test]
    fn rename_env_moves_files_toml_and_mapping() {
        let tmp = tempfile::tempdir().unwrap();
        // seed a 2-env project with a mapping.toml referencing `dev`
        let folder = tmp.path().join("acme");
        std::fs::create_dir_all(folder.join("envs/dev/queues")).unwrap();
        std::fs::write(folder.join("envs/dev/queues/x.json"), "{}").unwrap();
        std::fs::write(folder.join("rdc.toml"),
            "[envs.dev]\napi_base = \"https://d.test/api/v1\"\norg_id = 1\n\
             [envs.prod]\napi_base = \"https://p.test/api/v1\"\norg_id = 2\n").unwrap();
        rdc::secrets::write_secrets_file(&folder, "dev", "tok", None).unwrap();
        std::fs::write(folder.join("secrets/dev.hook-secrets.json"), "{}").unwrap();
        std::fs::create_dir_all(folder.join(".rdc")).unwrap();
        std::fs::write(folder.join(".rdc/mapping.toml"),
            "version = 2\n[[queues]]\ndev = \"cost-dev\"\nprod = \"cost-prod\"\n").unwrap();

        let p = rename_env(folder.display().to_string(), "dev".into(), "sandbox".into()).unwrap().project;
        let names: Vec<String> = p.envs.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"sandbox".to_string()) && names.contains(&"prod".to_string()));
        assert!(!names.contains(&"dev".to_string()));
        assert!(folder.join("envs/sandbox/queues/x.json").exists());
        assert!(!folder.join("envs/dev").exists());
        assert!(folder.join("secrets/sandbox.secrets.json").exists());
        assert!(folder.join("secrets/sandbox.hook-secrets.json").exists());
        assert!(!folder.join("secrets/dev.hook-secrets.json").exists());
        let mapping = std::fs::read_to_string(folder.join(".rdc/mapping.toml")).unwrap();
        assert!(mapping.contains("sandbox = \"cost-dev\"") && !mapping.contains("dev = \"cost-dev\""));
    }

    #[test]
    fn rename_env_returns_the_gitlab_follow_ups() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("acme");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("rdc.toml"),
            "[envs.dev]\napi_base = \"https://d.test/api/v1\"\norg_id = 1\n\
             [envs.prod]\napi_base = \"https://p.test/api/v1\"\norg_id = 2\n").unwrap();
        std::fs::write(folder.join(".gitlab-ci.yml"),
            "# >>> rdc:archive-envs\n- RDC_ENV: \"dev\"\n  RDC_VAR_SUFFIX: \"DEV\"\n# <<< rdc:archive-envs\n\
             # >>> rdc:deploy-jobs\n# <<< rdc:deploy-jobs\n").unwrap();
        let r = rename_env(folder.display().to_string(), "dev".into(), "sandbox".into()).unwrap();
        assert!(r.follow_ups[0].contains("RDC_TOKEN_SANDBOX"), "{:?}", r.follow_ups);
        assert!(r.project.envs.iter().any(|e| e.name == "sandbox"));
    }

    #[test]
    fn rename_env_to_the_same_name_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        let r = rename_env(folder.display().to_string(), "main".into(), "main".into()).unwrap();
        assert!(r.follow_ups.is_empty());
        assert_eq!(r.project.envs[0].name, "main");
    }

    #[test]
    fn rename_env_rejects_collision_and_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("acme");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("rdc.toml"),
            "[envs.dev]\napi_base = \"https://d.test/api/v1\"\norg_id = 1\n\
             [envs.prod]\napi_base = \"https://p.test/api/v1\"\norg_id = 2\n").unwrap();
        assert!(rename_env(folder.display().to_string(), "dev".into(), "prod".into()).is_err()); // collision
        assert!(rename_env(folder.display().to_string(), "dev".into(), "a/b".into()).is_err());  // invalid
        assert!(rename_env(folder.display().to_string(), "nope".into(), "x".into()).is_err());   // unknown old
    }

    #[test]
    fn rename_env_rejects_invalid_old_name() {
        // `old` is never user-typed in the normal UI flow (it comes from the
        // existing project's own env list), but `validate_existing_project`
        // adopts a hand-edited `rdc.toml` with *any* env key, so a malicious
        // or corrupted `old` must be rejected before it's interpolated into
        // any filesystem path — mirrors `remove_env_rejects_invalid_name`.
        let tmp = tempfile::tempdir().unwrap();
        let folder = seed_project(tmp.path(), "acme");
        for bad in ["../x", "a/b", ".."] {
            let err = rename_env(folder.display().to_string(), bad.into(), "sandbox".into()).unwrap_err();
            assert!(
                format!("{err:#}").contains("letters, digits"),
                "unexpected error for old={bad:?}: {err:#}"
            );
        }
        // Nothing was touched: the (legitimate) project still has only its
        // original env, untouched on disk.
        let cfg = rdc::config::ProjectConfig::load(&folder.join("rdc.toml")).unwrap();
        assert_eq!(cfg.envs.keys().collect::<Vec<_>>(), vec!["main"]);
    }

    // ---------------------------------------------------------------- SinkPromptRoute

    /// A `PromptSink` a test can inspect and "close", standing in for
    /// `StreamSink<SyncPhase>` (which needs a live Dart message-port handle
    /// and so cannot be constructed in a plain `#[test]`).
    struct FakeSink {
        emitted: std::sync::Mutex<Vec<SyncPhase>>,
        closed: std::sync::atomic::AtomicBool,
    }

    impl FakeSink {
        fn new() -> Self {
            Self {
                emitted: std::sync::Mutex::new(Vec::new()),
                closed: std::sync::atomic::AtomicBool::new(false),
            }
        }
        fn close(&self) {
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl PromptSink for FakeSink {
        fn emit(&self, phase: SyncPhase) -> bool {
            if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
                return false;
            }
            self.emitted.lock().unwrap().push(phase);
            true
        }
    }

    fn route_with_channel() -> (SinkPromptRoute<FakeSink>, std::sync::mpsc::Sender<(u64, String)>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let route = SinkPromptRoute {
            sink: FakeSink::new(),
            answers: std::sync::Mutex::new(rx),
            cancel: rdc::cli::sync::watch::CancelToken::new(),
            log_buf: LineBuffer::default(),
        };
        (route, tx)
    }

    /// Polls `route`'s `FakeSink` until a `SyncPhase::Prompt` has been
    /// emitted and returns its id. `ask()` mints that id internally (from
    /// the process-global `next_prompt_id()` counter), so a test that wants
    /// to send a correctly-tagged reply on the answer channel has no way to
    /// know it in advance — it has to read it back off the sink.
    fn wait_for_prompt_id(route: &SinkPromptRoute<FakeSink>) -> u64 {
        loop {
            if let Some(SyncPhase::Prompt { id, .. }) = route.sink.emitted.lock().unwrap().last() {
                return *id;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn sample_prompt() -> rdc::cli::sync::embed::Prompt {
        rdc::cli::sync::embed::Prompt {
            kind: rdc::cli::sync::embed::PromptKind::DeleteGate,
            question: "Proceed with deletion? ".into(),
            keys: vec![
                rdc::cli::sync::embed::PromptKey::new('y', "yes"),
                rdc::cli::sync::embed::PromptKey::new('e', "edit"),
                rdc::cli::sync::embed::PromptKey::new('h', "hunk-by-hunk"),
                rdc::cli::sync::embed::PromptKey::new('n', "no"),
            ],
        }
    }

    #[test]
    fn ask_drains_a_stale_answer_before_waiting() {
        let (route, tx) = route_with_channel();
        // Leftover from an earlier, already-resolved prompt. Tagged with an
        // id that will never match (0, and `next_prompt_id()` starts at 1)
        // AND offers a key `sample_prompt` actually has ("n", not e.g.
        // "stale") — either the drain or the id check alone would reject
        // it, so this on its own doesn't prove the drain still runs; it's
        // the id-mismatch test below that isolates that check. This test
        // stays about the drain: it must never even reach the id check for
        // this stale entry.
        tx.send((0, "n".to_string())).unwrap();
        std::thread::scope(|scope| {
            let sender = tx.clone();
            let route_ref = &route;
            scope.spawn(move || {
                let id = wait_for_prompt_id(route_ref);
                std::thread::sleep(std::time::Duration::from_millis(30));
                sender.send((id, "y".to_string())).unwrap();
            });
            // If the stale answer were not drained, this would return "n"
            // immediately instead of blocking for the real "y".
            assert_eq!(route.ask(&sample_prompt()), Some("y".to_string()));
        });
    }

    #[test]
    fn ask_ignores_an_answer_for_a_different_prompt_id() {
        // Restoring `prompt_id` to `answer_prompt` is only meaningful if
        // `ask` actually checks it: a reply tagged for some OTHER prompt
        // (an earlier one, already superseded) must never be applied here,
        // even though its key ("y") is perfectly valid.
        let (route, tx) = route_with_channel();
        std::thread::scope(|scope| {
            let sender = tx.clone();
            let route_ref = &route;
            scope.spawn(move || {
                let id = wait_for_prompt_id(route_ref);
                sender.send((id.wrapping_add(1), "y".to_string())).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(30));
                sender.send((id, "n".to_string())).unwrap();
            });
            assert_eq!(route.ask(&sample_prompt()), Some("n".to_string()));
        });
    }

    #[test]
    fn ask_never_offers_e_or_h() {
        let (route, tx) = route_with_channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let id = wait_for_prompt_id(&route);
                tx.send((id, "y".to_string())).unwrap();
            });
            let _ = route.ask(&sample_prompt());
        });
        let emitted = route.sink.emitted.lock().unwrap();
        match &emitted[0] {
            SyncPhase::Prompt { keys, .. } => {
                let offered: Vec<&str> = keys.iter().map(|k| k.key.as_str()).collect();
                assert_eq!(offered, vec!["y", "n"], "e/h must be stripped from what the UI sees");
            }
            other => panic!("expected a Prompt phase, got {other:?}"),
        }
    }

    #[test]
    fn ask_ignores_an_answer_outside_the_offered_keys() {
        // `e` is a real key the CORE offered but that this bridge strips —
        // it must never be accepted even if it somehow arrives on the
        // answer channel (a future UI bug, not reachable today).
        let (route, tx) = route_with_channel();
        std::thread::scope(|scope| {
            let sender = tx.clone();
            let route_ref = &route;
            scope.spawn(move || {
                let id = wait_for_prompt_id(route_ref);
                sender.send((id, "e".to_string())).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
                sender.send((id, "y".to_string())).unwrap();
            });
            assert_eq!(route.ask(&sample_prompt()), Some("y".to_string()));
        });
    }

    #[test]
    fn ask_degrades_to_none_when_the_sink_is_closed() {
        let (route, _tx) = route_with_channel();
        route.sink.close();
        // No answer will ever arrive (the sender is kept alive by `_tx`, so
        // this can't return via a closed-channel `Disconnected` either) —
        // the only way `ask` returns is the initial `emit` failing and
        // short-circuiting before the wait loop is ever entered. If it
        // didn't, this call would hang.
        assert_eq!(route.ask(&sample_prompt()), None);
    }

    #[test]
    fn ask_unblocks_when_cancelled_while_parked() {
        let (route, _tx) = route_with_channel();
        let cancel = route.cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            cancel.cancel();
        });
        // No answer is ever sent; without the cancellation check this would
        // hang forever rather than returning within one poll interval.
        assert_eq!(route.ask(&sample_prompt()), None);
    }

    #[test]
    fn ask_discards_the_forwarders_buffered_partial_question_line() {
        // Regression test for the log-pane corruption: every prompt writes
        // its question with NO trailing newline (correct for a terminal,
        // where the answer is typed on that line), so `LineForwarder`'s
        // buffer holds exactly that text at the moment `ask()` is about to
        // block. Without the discard, it would sit there and get prepended
        // to whatever real log line arrives once the prompt is answered.
        use std::io::Write as _;

        let log_buf = LineBuffer::default();
        let mut forwarder = LineForwarder {
            sink: FakeSink::new(),
            buf: log_buf.clone(),
        };
        forwarder
            .write_all(b"[k] keep local  [r] use dev  [a] abort > ")
            .unwrap();
        assert!(
            forwarder.sink.emitted.lock().unwrap().is_empty(),
            "no newline yet -- nothing should have been forwarded as a Log line"
        );

        let route = SinkPromptRoute {
            sink: FakeSink::new(),
            answers: std::sync::Mutex::new(std::sync::mpsc::channel().1),
            cancel: rdc::cli::sync::watch::CancelToken::new(),
            log_buf: log_buf.clone(),
        };
        // Pre-cancelled: `ask` still runs the drain/discard/emit sequence
        // before its wait loop ever checks this, so no second thread is
        // needed to unblock it.
        route.cancel.cancel();
        assert_eq!(route.ask(&sample_prompt()), None);

        // The question text must be gone -- discarded, not forwarded -- so
        // it can never get prepended to the next real line.
        forwarder
            .write_all(b"12:01:14 pull  queues/invoices\n")
            .unwrap();
        let emitted = forwarder.sink.emitted.lock().unwrap();
        match &emitted[0] {
            SyncPhase::Log { line } => {
                assert_eq!(line, "12:01:14 pull  queues/invoices");
            }
            other => panic!("expected a Log phase, got {other:?}"),
        }
    }

    #[test]
    fn kind_to_dto_maps_every_variant_explicitly() {
        use rdc::cli::sync::embed::PromptKind as K;
        assert_eq!(kind_to_dto(K::Conflict), PromptKindDto::Conflict);
        assert_eq!(kind_to_dto(K::RemoteDelete), PromptKindDto::RemoteDelete);
        assert_eq!(kind_to_dto(K::BulkConfirm), PromptKindDto::BulkConfirm);
        assert_eq!(kind_to_dto(K::DeleteGate), PromptKindDto::DeleteGate);
        assert_eq!(kind_to_dto(K::DeleteDrift), PromptKindDto::DeleteDrift);
        assert_eq!(kind_to_dto(K::MdhIndexDrop), PromptKindDto::MdhIndexDrop);
        assert_eq!(kind_to_dto(K::MdhRowDelete), PromptKindDto::MdhRowDelete);
        assert_eq!(kind_to_dto(K::Unknown), PromptKindDto::Unknown);
    }
}
