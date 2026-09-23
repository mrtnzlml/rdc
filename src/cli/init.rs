use crate::config::{EnvConfig, ProjectConfig};
use crate::paths::Paths;
use crate::snapshot::writer::write_atomic;
use anyhow::{anyhow, Context, Result};
use inquire::error::InquireError;
use inquire::validator::Validation;
use inquire::{Confirm, CustomType, Text};
use std::io::IsTerminal;
use std::path::Path;

/// Whether `rdc init` is bootstrapping a new project or extending an
/// existing one by adding new env(s).
enum InitMode {
    New,
    Extend,
}

/// What a scaffold writer did to its file, for the `--force` summary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Scaffolded {
    Created,
    /// Existing file replaced with the current template (`--force` only).
    Rewritten,
    /// Missing canonical lines appended, user lines kept
    /// (`.gitignore` / `.gitattributes`).
    Merged,
    Unchanged,
}

impl Scaffolded {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Scaffolded::Created => "created",
            Scaffolded::Rewritten => "rewritten",
            Scaffolded::Merged => "updated",
            Scaffolded::Unchanged => "unchanged",
        }
    }
}

pub async fn run(env_specs: Vec<String>, force: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let cfg_path = cwd.join("rdc.toml");

    let (mut cfg, mode) = if cfg_path.exists() {
        let existing = ProjectConfig::load(&cfg_path).with_context(|| {
            format!("loading existing project config from {}", cfg_path.display())
        })?;
        (existing, InitMode::Extend)
    } else {
        (ProjectConfig::default(), InitMode::New)
    };

    // `--force` with no `--env` is regenerate-only: refresh the scaffold files
    // from this binary's templates and stop — no env wizard, no auth, no sync,
    // and `rdc.toml` is left exactly as it is. There is nothing to regenerate
    // without a project, so that combination is a usage error rather than a
    // silent fall-through into the bootstrap wizard.
    let regenerate_only = force && env_specs.is_empty();
    if regenerate_only && matches!(mode, InitMode::New) {
        return Err(anyhow!(
            "rdc init --force: no rdc.toml in {} — nothing to regenerate. \
             Bootstrap the project first: rdc init --env <env>=<api_base>:<org_id>",
            cwd.display()
        ));
    }

    // Get env specs (interactive when none provided + TTY available).
    let env_specs = if regenerate_only {
        Vec::new()
    } else {
        resolve_env_specs(env_specs, &cfg, &mode)?
    };

    let mut new_env_names: Vec<String> = Vec::new();
    for spec in &env_specs {
        let (env_name, env_cfg) = parse_env_spec(spec)?;
        if cfg.envs.contains_key(&env_name) {
            return Err(anyhow!(
                "env '{env_name}' already exists in this project; \
                 to update it, edit {} directly",
                cfg_path.display()
            ));
        }
        // Reject names that normalize to the same `RDC_TOKEN_<...>`
        // variable as an existing env. E.g. adding `dev_us` when
        // `dev-us` is already defined would silently share env-var
        // resolution; refuse rather than let one steal the other's
        // token.
        let candidate_var = crate::secrets::env_var_for(&env_name, "TOKEN");
        if let Some(clash) = cfg
            .envs
            .keys()
            .find(|existing| crate::secrets::env_var_for(existing, "TOKEN") == candidate_var)
        {
            return Err(anyhow!(
                "env '{env_name}' would share API-token env var '{candidate_var}' with \
                 existing env '{clash}' (rdc normalizes non-alphanumerics to '_' so \
                 the shell can export it). Pick a distinct name."
            ));
        }
        cfg.envs.insert(env_name.clone(), env_cfg);
        new_env_names.push(env_name);
    }

    // Nothing was added in regenerate-only mode, so don't rewrite `rdc.toml`:
    // a save round-trips it through `ProjectConfig` and would drop any key this
    // version doesn't model.
    if !regenerate_only {
        cfg.save(&cfg_path)?;
    }

    let mut scaffold: Vec<(String, Scaffolded)> = vec![
        (".gitignore".into(), write_gitignore(&cwd)?),
        (".gitattributes".into(), write_gitattributes(&cwd)?),
        ("CLAUDE.md".into(), write_claude_md(&cwd, &cfg, force)?),
        ("README.md".into(), write_readme(&cwd, &cfg, force)?),
        (".gitlab-ci.yml".into(), write_gitlab_ci(&cwd, &cfg, force)?),
    ];
    scaffold.extend(write_testkit(&cwd, force)?.into_iter().map(|(n, o)| (n.into(), o)));
    std::fs::create_dir_all(cwd.join("secrets"))
        .with_context(|| format!("creating {}", cwd.join("secrets").display()))?;
    for env in &new_env_names {
        let paths = Paths::for_env(&cwd, env);
        std::fs::create_dir_all(paths.env_root())
            .with_context(|| format!("creating {}", paths.env_root().display()))?;
        std::fs::create_dir_all(paths.hooks_dir())
            .with_context(|| format!("creating {}", paths.hooks_dir().display()))?;
    }
    // Every env, not just the ones this run added: a project that predates
    // these two files gets them on its next `rdc init` (including a bare
    // `--force` regenerate) rather than only when it happens to add an env.
    for env in cfg.envs.keys() {
        scaffold.extend(write_env_scaffolds(&cwd, env)?);
    }

    // The per-file summary is the whole output of a regenerate-only run, and
    // the receipt a `--force` user needs elsewhere ("did it touch my README?").
    // Silent otherwise, so a plain init keeps its one-line output.
    if force {
        println!("Scaffold files:");
        for (name, outcome) in &scaffold {
            println!("  {name:<30} {}", outcome.label());
        }
    }

    let env_list = new_env_names.join(", ");
    match mode {
        InitMode::New => {
            println!("Initialized rdc project with envs: {env_list}");
        }
        InitMode::Extend if !regenerate_only => {
            println!("Added env(s): {env_list}");
        }
        InitMode::Extend => {}
    }

    // Auth-on-init: for each new env, try to authenticate up front so the
    // user doesn't have to make a second pass through `rdc auth`. Token
    // sources, in order: `RDC_TOKEN_<UPPER>` env var (non-interactive,
    // CI-friendly), then masked TTY prompt (when stdin is a terminal).
    // Validation goes through the same `validate_and_save_token` helper
    // `rdc auth` uses, so the on-disk state and printed feedback match.
    let mut auth_succeeded: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for env in &new_env_names {
        let env_var_name = crate::secrets::env_var_for(env, "TOKEN");
        let token_source = match std::env::var(&env_var_name) {
            Ok(t) if !t.trim().is_empty() => Some((t.trim().to_string(), env_var_name.clone())),
            _ => {
                if std::io::stdin().is_terminal() {
                    prompt_token_for_env(env)?.map(|t| (t, "interactive prompt".to_string()))
                } else {
                    None
                }
            }
        };

        let Some((token, source_label)) = token_source else {
            continue;
        };
        let env_cfg = cfg
            .envs
            .get(env)
            .expect("just inserted into cfg above");
        match crate::cli::auth::validate_and_save_token(env_cfg, &cwd, env, &token)
            .await
        {
            Ok(_org_name) => {
                auth_succeeded.insert(env.clone());
            }
            Err(e) => {
                // Project files stay; user can rerun `rdc auth` once
                // they've sorted out the credential issue.
                let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode());
                log.event(
                    crate::log::Action::Warn,
                    &format!("token for env '{env}' (from {source_label}) failed validation: {e:#}; re-run `rdc auth {env}` to retry"),
                );
            }
        }
    }

    // Sync-on-init: once auth has succeeded for one or more new envs,
    // the very next manual step the user would run is `rdc sync <env>`.
    // Offer to do that here so the happy path is a single command. TTY
    // gated only, matching the rest of the init wizard. Default Yes —
    // the user just configured the env, sync is what they came for.
    //
    // Each env is synced independently; one failure doesn't abort the
    // others (or invalidate the project files already on disk). Envs
    // whose sync failed fall back to the manual `rdc sync <env>` line
    // in the Next steps block below.
    let syncable: Vec<String> = new_env_names
        .iter()
        .filter(|e| auth_succeeded.contains(*e))
        .cloned()
        .collect();
    let mut synced: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if !syncable.is_empty() && std::io::stdin().is_terminal() {
        let prompt_label = if syncable.len() == 1 {
            format!("Sync '{}' now?", syncable[0])
        } else {
            format!("Sync the {} new env(s) now?", syncable.len())
        };
        let want_sync = match Confirm::new(&prompt_label)
            .with_default(true)
            .with_help_message("pulls the remote snapshot into envs/<env>/ — you can always run `rdc sync` later")
            .prompt()
        {
            Ok(b) => b,
            // Esc / Ctrl+C on the confirm = "don't sync now". Falls
            // through to the next-steps message; project files stay.
            Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => false,
            Err(e) => return Err(anyhow!("prompt failed: {e}")),
        };
        if want_sync {
            for env in &syncable {
                println!();
                match crate::cli::sync::run(env, true, false, false, false, false, None).await {
                    Ok(_outcome) => {
                        synced.insert(env.clone());
                    }
                    Err(e) => {
                        let log =
                            crate::log::Log::new(crate::cli::resolve::detect_color_mode());
                        log.event(
                            crate::log::Action::Warn,
                            &format!(
                                "sync of env '{env}' failed: {e:#}; re-run `rdc sync {env}` later"
                            ),
                        );
                    }
                }
            }
        }
    }

    // Only show Next steps when there's actually something left to do.
    // After a clean "init → auth → sync" run, every env is fully set up
    // and a trailing empty header would feel like a dangling todo list.
    let needs_token_step = new_env_names.iter().any(|e| !auth_succeeded.contains(e));
    let needs_sync_step = new_env_names.iter().any(|e| !synced.contains(e));
    if needs_token_step || needs_sync_step {
        println!();
        println!("Next steps:");
        for env in &new_env_names {
            if auth_succeeded.contains(env) {
                continue;
            }
            let env_var_name = crate::secrets::env_var_for(env, "TOKEN");
            println!("  - Set the API token for env '{env}':");
            println!("      rdc auth {env} --token <token>     # validates + writes secrets/{env}.secrets.json");
            println!("      # or: export {env_var_name}=<token>");
        }
        for env in &new_env_names {
            if synced.contains(env) {
                continue;
            }
            println!("  - Sync the snapshot:  rdc sync {env}");
        }
    }
    Ok(())
}

/// Prompt for an API token on TTY (masked). Returns `Ok(None)` when the
/// user cancels (Ctrl+C / Esc) or submits an empty value — both mean
/// "don't auth right now; fall back to the next-steps message and let
/// `rdc auth` handle it later". Hard errors (real I/O failures from the
/// prompt library) propagate.
fn prompt_token_for_env(env: &str) -> Result<Option<String>> {
    use inquire::error::InquireError;
    use inquire::{Password, PasswordDisplayMode};

    println!();
    let result = Password::new(&format!("API token for '{env}'"))
        .with_display_mode(PasswordDisplayMode::Masked)
        .without_confirmation()
        .with_help_message("Esc / Ctrl+C to skip — you can run `rdc auth` later")
        .prompt();
    match result {
        Ok(t) => {
            let trimmed = t.trim().to_string();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed))
            }
        }
        Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => Ok(None),
        Err(e) => Err(anyhow!("token prompt failed: {e}")),
    }
}

/// Resolve the list of env specs to use. When `env_specs` is empty,
/// prompts interactively (TTY-only). Invalid input re-prompts inline
/// rather than aborting; Esc or Ctrl+C cancels.
fn resolve_env_specs(
    env_specs: Vec<String>,
    cfg: &ProjectConfig,
    mode: &InitMode,
) -> Result<Vec<String>> {
    if !env_specs.is_empty() {
        return Ok(env_specs);
    }
    if !std::io::stdin().is_terminal() {
        let example = "rdc init --env dev=https://api.elis.rossum.ai/v1:123456";
        let msg = match mode {
            InitMode::New => format!(
                "rdc init: at least one --env is required when stdin is not a TTY. \
                 Example: {example}"
            ),
            InitMode::Extend => format!(
                "rdc init: at least one --env is required when stdin is not a TTY \
                 (extending existing project). Example: {example}"
            ),
        };
        return Err(anyhow!(msg));
    }

    println!();
    match mode {
        InitMode::New => {
            println!("Set up a new rdc project.");
        }
        InitMode::Extend => {
            let existing = if cfg.envs.is_empty() {
                "(none)".to_string()
            } else {
                cfg.envs.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            println!("Add environments to existing project. Existing: {existing}.");
        }
    }
    println!();

    let mut specs: Vec<String> = Vec::new();
    let mut taken: Vec<String> = cfg.envs.keys().cloned().collect();

    loop {
        let env_name = match prompt_env_name(&taken) {
            Ok(s) => s,
            Err(PromptOutcome::Cancelled) => {
                return finish_or_cancel(specs);
            }
            Err(PromptOutcome::Failed(e)) => return Err(e),
        };

        // Ask org id before api_base: env name + org id are the identity
        // facts a user knows offhand, so leading with them feels more
        // natural than the connection URL. The API token is gathered later
        // (the auth phase in `run`), so the user-facing question order is
        // env name -> org id -> api_base -> token.
        let org_id = match prompt_org_id() {
            Ok(n) => n,
            Err(PromptOutcome::Cancelled) => {
                return finish_or_cancel(specs);
            }
            Err(PromptOutcome::Failed(e)) => return Err(e),
        };

        let api_base = match prompt_api_base() {
            Ok(s) => s,
            Err(PromptOutcome::Cancelled) => {
                return finish_or_cancel(specs);
            }
            Err(PromptOutcome::Failed(e)) => return Err(e),
        };

        taken.push(env_name.clone());
        specs.push(format!("{env_name}={api_base}:{org_id}"));

        match Confirm::new("Add another environment?")
            .with_default(false)
            .prompt()
        {
            Ok(true) => continue,
            Ok(false) => break,
            // Esc / Ctrl+C on the confirm = "I'm done, don't add another".
            Err(InquireError::OperationCanceled) | Err(InquireError::OperationInterrupted) => break,
            Err(e) => return Err(anyhow!("prompt failed: {e}")),
        }
    }

    if specs.is_empty() {
        return Err(anyhow!("at least one env is required"));
    }
    Ok(specs)
}

enum PromptOutcome {
    Cancelled,
    Failed(anyhow::Error),
}

fn finish_or_cancel(specs: Vec<String>) -> Result<Vec<String>> {
    if specs.is_empty() {
        Err(anyhow!("init cancelled; no environments defined"))
    } else {
        Ok(specs)
    }
}

fn map_prompt_err(e: InquireError) -> PromptOutcome {
    match e {
        InquireError::OperationCanceled | InquireError::OperationInterrupted => {
            PromptOutcome::Cancelled
        }
        other => PromptOutcome::Failed(anyhow!("prompt failed: {other}")),
    }
}

fn prompt_env_name(taken: &[String]) -> std::result::Result<String, PromptOutcome> {
    let taken_owned: Vec<String> = taken.to_vec();
    let value = Text::new("Env name")
        .with_help_message("short slug, e.g. dev, staging, prod (Esc to finish)")
        .with_validator(move |input: &str| {
            let trimmed = input.trim();
            if trimmed.is_empty() {
                return Ok(Validation::Invalid("env name cannot be empty".into()));
            }
            if !trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Ok(Validation::Invalid(
                    "only letters, digits, '-', and '_' are allowed".into(),
                ));
            }
            if taken_owned.iter().any(|n| n == trimmed) {
                return Ok(Validation::Invalid(
                    format!("'{trimmed}' is already defined").into(),
                ));
            }
            Ok(Validation::Valid)
        })
        .prompt()
        .map_err(map_prompt_err)?;
    Ok(value.trim().to_string())
}

fn prompt_api_base() -> std::result::Result<String, PromptOutcome> {
    let value = Text::new("API base URL")
        .with_help_message("e.g. https://api.elis.rossum.ai/v1")
        .with_validator(|input: &str| {
            let trimmed = input.trim();
            if trimmed.is_empty() {
                return Ok(Validation::Invalid("api_base cannot be empty".into()));
            }
            if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
                return Ok(Validation::Invalid(
                    "must start with http:// or https://".into(),
                ));
            }
            Ok(Validation::Valid)
        })
        .prompt()
        .map_err(map_prompt_err)?;
    Ok(value.trim().to_string())
}

fn prompt_org_id() -> std::result::Result<u64, PromptOutcome> {
    CustomType::<u64>::new("Organization ID")
        .with_help_message("Rossum organization ID (positive integer)")
        .with_error_message("must be a positive integer")
        .prompt()
        .map_err(map_prompt_err)
}

fn parse_env_spec(spec: &str) -> Result<(String, EnvConfig)> {
    let (env_name, rest) = spec
        .split_once('=')
        .ok_or_else(|| anyhow!("invalid --env spec '{spec}': expected `<env>=<api_base>:<org_id>`"))?;
    let last_colon = rest
        .rfind(':')
        .ok_or_else(|| anyhow!("invalid --env spec '{spec}': missing :<org_id>"))?;
    let api_base = &rest[..last_colon];
    let org_id_str = &rest[last_colon + 1..];
    let org_id: u64 = org_id_str
        .parse()
        .with_context(|| format!("parsing org_id '{org_id_str}' in spec '{spec}'"))?;
    Ok((
        env_name.to_string(),
        EnvConfig {
            api_base: api_base.to_string(),
            org_id,
        },
    ))
}

/// Ensure the canonical ignore patterns are present. Additive by design —
/// including under `--force`, which is why it takes no `force` flag: the merge
/// already restores every rdc-owned line without discarding the user's own.
fn write_gitignore(root: &Path) -> Result<Scaffolded> {
    let path = root.join(".gitignore");
    // Canonical patterns this template ensures. Each line is checked
    // independently so re-running `rdc init` on a project that already
    // has *some* of these (or has them with surrounding annotation)
    // adds only the missing lines instead of dumping the whole block
    // and creating duplicates.
    //
    // `/.rdc/state/*.lock` — sibling of the `<env>.lock.json` lockfile.
    // The `.lock` file is the empty advisory lock fs4 grabs an OS-level
    // exclusive lock on (see `cli::sync::lock::EnvLock`). The body is
    // always empty by design and the contents are per-machine, so it
    // shouldn't be committed. `*.lock` is narrow enough not to match
    // `.lock.json` (that's the actual committable lockfile state).
    //
    // `/.rdc/state/*.base` — sync's 3-way merge keeps a per-machine
    // sidecar cache at `.rdc/state/<env>.base/` mirroring the env
    // tree at the moment of the last successful sync (see
    // `state::base_cache`). Like the advisory lock file, it's
    // local-only and regenerated on the next sync, so it must not
    // be committed.
    //
    // `/.rdc/conflicts` — sync parks the remote side of an unresolved
    // conflict (and its `-deleted` marker) under `.rdc/conflicts/<env>/`
    // (see `paths::Paths::conflict_shadow_path`). These are transient
    // review artifacts consumed when the conflict is resolved, so they
    // must not be committed.
    const PATTERNS: &[&str] = &[
        "/target",
        "/secrets",
        "/.rdc/cache",
        "/.rdc/state/*.lock",
        "/.rdc/state/*.base",
        "/.rdc/conflicts",
        // pytest + CPython leave these beside the scaffolded testkit.
        "__pycache__/",
        "/.pytest_cache",
    ];

    if !path.exists() {
        let body: String = PATTERNS.iter().map(|p| format!("{p}\n")).collect();
        write_atomic(&path, body.as_bytes())?;
        return Ok(Scaffolded::Created);
    }

    let existing = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let existing_lines: std::collections::HashSet<&str> =
        existing.lines().map(str::trim).collect();

    let missing: Vec<&str> = PATTERNS
        .iter()
        .copied()
        .filter(|p| !existing_lines.contains(p))
        .collect();
    if missing.is_empty() {
        return Ok(Scaffolded::Unchanged);
    }

    let mut combined = existing;
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    for p in missing {
        combined.push_str(p);
        combined.push('\n');
    }
    write_atomic(&path, combined.as_bytes())?;
    Ok(Scaffolded::Merged)
}

/// Mark rdc-owned files under `.rdc/` as generated so GitHub collapses
/// their diffs by default and excludes them from language stats. The
/// state lockfile (`state/<env>.lock.json`) and the cross-env slug map
/// (`mapping.toml`) are produced by the tool; reviewers
/// shouldn't have to scroll past them.
/// Additive like [`write_gitignore`], and for the same reason.
fn write_gitattributes(root: &Path) -> Result<Scaffolded> {
    let path = root.join(".gitattributes");
    let body = ".rdc/** linguist-generated=true\n";
    if path.exists() {
        let existing = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if existing.contains(".rdc/** linguist-generated") {
            return Ok(Scaffolded::Unchanged);
        }
        let mut combined = existing;
        if !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(body);
        write_atomic(&path, combined.as_bytes())?;
        Ok(Scaffolded::Merged)
    } else {
        write_atomic(&path, body.as_bytes())?;
        Ok(Scaffolded::Created)
    }
}

/// Write `body` at `path` with the scaffold contract: a missing file is
/// created, an existing one is left alone unless `force`. Under `--force` the
/// bytes are compared first, so re-running on an untouched project reports
/// `Unchanged` instead of churning mtimes (and the comparison is byte-wise, so
/// a hand-edited file that isn't valid UTF-8 doesn't fail the run) --
/// delegated to [`write_template_file_bytes`] once the existing bytes (if
/// any) are in hand.
fn write_template_file(path: &Path, body: &str, force: bool) -> Result<Scaffolded> {
    if !path.exists() {
        write_atomic(path, body.as_bytes())?;
        return Ok(Scaffolded::Created);
    }
    let existing = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    write_template_file_bytes(path, body.as_bytes(), &existing, force)
}

/// Write the GitLab CI pipeline at `<root>/.gitlab-ci.yml`.
///
/// The body is the repo's `templates/gitlab-ci.yml`, embedded at compile time
/// so the copy users read on GitHub and the copy this binary writes cannot
/// drift — with the two `# >>> rdc:…` regions filled in from `cfg` (the archive
/// matrix and one drafted deploy button per env).
///
/// | file state                  | action                        | outcome     |
/// |-----------------------------|-------------------------------|-------------|
/// | absent                      | write the generated template  | `Created`   |
/// | has markers                 | splice; bytes equal           | `Unchanged` |
/// | has markers                 | splice; bytes differ          | `Merged`    |
/// | no markers, no `--force`    | leave alone                   | `Unchanged` |
/// | no markers, `--force`       | regenerate the whole file     | `Rewritten` |
///
/// A markered file is spliced even under `--force`: the lines outside the
/// markers are the user's, and `Merged` already means "rdc-owned lines
/// refreshed, user lines kept" for `.gitignore`. To take a newer binary's
/// static half, delete the file and re-run `rdc init`.
///
/// The splice is not symmetric across the two regions, and that asymmetry is
/// the point: the archive matrix is re-rendered from `rdc.toml`, while the
/// deploy jobs are only appended to, so a filled-in `RDC_SRC` survives and a
/// deleted draft stays deleted (see
/// [`crate::cli::gitlab_ci::render_regions_for_existing`]).
///
/// With no envs defined (a hand-emptied `rdc.toml`), the template is written
/// verbatim: an empty `parallel:matrix` is not valid YAML, and its committed
/// example is.
fn write_gitlab_ci(root: &Path, cfg: &ProjectConfig, force: bool) -> Result<Scaffolded> {
    let path = root.join(".gitlab-ci.yml");
    if cfg.envs.is_empty() {
        return write_template_file(&path, GITLAB_CI_TEMPLATE, force);
    }

    let generated = || crate::cli::gitlab_ci::generate(GITLAB_CI_TEMPLATE, &cfg.envs);

    if !path.exists() {
        write_atomic(&path, generated()?.as_bytes())?;
        return Ok(Scaffolded::Created);
    }

    let existing =
        std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    // A hand-edited pipeline that isn't valid UTF-8 can't be spliced; treat it
    // like write_template_file does — byte comparison, never a parse.
    let Ok(text) = String::from_utf8(existing.clone()) else {
        return write_template_file_bytes(&path, generated()?.as_bytes(), &existing, force);
    };

    // Additive for the deploy jobs, re-rendered for the archive matrix.
    let regions = crate::cli::gitlab_ci::render_regions_for_existing(&text, &cfg.envs);
    match crate::cli::regions::splice(&text, &regions, crate::cli::regions::YAML)
        .with_context(|| format!("updating the rdc regions in {}", path.display()))?
    {
        Some(spliced) => {
            if spliced.as_bytes() == existing.as_slice() {
                Ok(Scaffolded::Unchanged)
            } else {
                write_atomic(&path, spliced.as_bytes())?;
                Ok(Scaffolded::Merged)
            }
        }
        None => write_template_file_bytes(&path, generated()?.as_bytes(), &existing, force),
    }
}

/// Write the Python test harness and its pytest wiring. Same scaffold contract
/// as every other template: each file is created when absent and replaced only
/// under `--force`, so a project that has evolved its own copy keeps it.
///
/// The harness's own self-tests ship with it on purpose: `pytest -q` with
/// nothing collected exits 5, which would make the pipeline's test job red on a
/// project that has not written any tests yet.
fn write_testkit(root: &Path, force: bool) -> Result<Vec<(&'static str, Scaffolded)>> {
    std::fs::create_dir_all(root.join("testkit"))
        .with_context(|| format!("creating {}", root.join("testkit").display()))?;
    TESTKIT_TEMPLATES
        .iter()
        .map(|(rel, body)| Ok((*rel, write_template_file(&root.join(rel), body, force)?)))
        .collect()
}

/// Write the two per-env files a user would otherwise have to know exist:
/// `envs/<env>/overlay.toml` and `secrets/<env>.hook-secrets.json`. Both are
/// written as self-documenting, inert stubs — the overlay declares only
/// `version = 1` and comments, the secrets file only an empty `hooks` map —
/// so a project that never touches either behaves exactly as it did before.
///
/// **Create-if-absent, with no `--force` escape hatch**, unlike every other
/// scaffold. `overlay.toml` holds hand-written per-env overrides and the
/// secrets file holds values that exist nowhere else (Rossum never returns
/// them, and the file is gitignored), so for these two there is no version of
/// "regenerate it from the template" that is not data loss.
///
/// `rdc sync` calls this too. The overlay is committed, so `rdc init` alone
/// would be enough for it — but the secrets file is gitignored, so a clone
/// would never inherit one, and an existing project only re-runs `init` when
/// it adds an env.
pub(crate) fn write_env_scaffolds(
    root: &Path,
    env: &str,
) -> Result<Vec<(String, Scaffolded)>> {
    let paths = Paths::for_env(root, env);

    let overlay = paths.overlay_file();
    let overlay_outcome = if overlay.exists() {
        Scaffolded::Unchanged
    } else {
        write_atomic(&overlay, OVERLAY_TEMPLATE.as_bytes())?;
        Scaffolded::Created
    };

    let created = crate::secrets::write_hook_secrets_stub(root, env)?;
    let secrets_outcome = if created { Scaffolded::Created } else { Scaffolded::Unchanged };

    Ok(vec![
        (format!("envs/{env}/overlay.toml"), overlay_outcome),
        (format!("secrets/{env}.hook-secrets.json"), secrets_outcome),
    ])
}

/// `write_template_file`'s force semantics against bytes already in hand.
fn write_template_file_bytes(
    path: &Path,
    body: &[u8],
    existing: &[u8],
    force: bool,
) -> Result<Scaffolded> {
    if !force {
        return Ok(Scaffolded::Unchanged);
    }
    if existing == body {
        return Ok(Scaffolded::Unchanged);
    }
    write_atomic(path, body)?;
    Ok(Scaffolded::Rewritten)
}

/// Write an agent guide at `<root>/CLAUDE.md`.
///
/// Mostly a static template, with two generated regions (the env table and the
/// promote walkthrough) filled from `cfg`. Like the pipeline, an existing file
/// carrying the markers has only its regions refreshed — so a project's own
/// notes survive every `rdc init` — and a file with no markers is left alone
/// unless `--force`.
fn write_claude_md(root: &Path, cfg: &ProjectConfig, force: bool) -> Result<Scaffolded> {
    write_doc_with_regions(&root.join("CLAUDE.md"), CLAUDE_MD_TEMPLATE, cfg, force)
}

/// Shared body for the two Markdown scaffolds: generate from `template` when
/// absent, splice the rdc regions when present, honour `--force` for a file
/// that has no markers at all.
fn write_doc_with_regions(
    path: &Path,
    template: &str,
    cfg: &ProjectConfig,
    force: bool,
) -> Result<Scaffolded> {
    let regions = crate::cli::scaffold_docs::render_doc_regions(&cfg.envs);
    let style = crate::cli::regions::MARKDOWN;
    let generated = || -> Result<String> {
        crate::cli::regions::splice(template, &regions, style)?.ok_or_else(|| {
            anyhow!(
                "the embedded template for {} has no rdc region markers",
                path.display()
            )
        })
    };

    if !path.exists() {
        let body = generated()?;
        write_atomic(path, body.as_bytes())?;
        return Ok(Scaffolded::Created);
    }

    let existing = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let Ok(text) = String::from_utf8(existing.clone()) else {
        return write_template_file_bytes(path, generated()?.as_bytes(), &existing, force);
    };
    match crate::cli::regions::splice(&text, &regions, style)
        .with_context(|| format!("updating the rdc regions in {}", path.display()))?
    {
        Some(spliced) => {
            if spliced.as_bytes() == existing.as_slice() {
                Ok(Scaffolded::Unchanged)
            } else {
                write_atomic(path, spliced.as_bytes())?;
                Ok(Scaffolded::Merged)
            }
        }
        None => write_template_file_bytes(path, generated()?.as_bytes(), &existing, force),
    }
}

/// Write a human-facing `README.md` at the project root.
///
/// Every env-derived line lives inside a generated region — the
/// `rdc sync <env>` list (`rdc:sync`), the environment table (`rdc:envs`) and
/// the promote recipe (`rdc:promote`, the same body `CLAUDE.md` gets, rendered
/// once in [`crate::cli::scaffold_docs`]). That is not decoration: on an
/// existing README only the regions are spliced, so an env-derived line
/// *outside* one would freeze at whatever `rdc.toml` said the day the file was
/// created — a table listing two envs above a command block naming one.
///
/// Every region is emitted unconditionally, including for a hand-emptied
/// `rdc.toml`: [`write_doc_with_regions`] hard-errors on a template with no
/// markers, and each renderer has an explanatory body for the empty case.
///
/// Same scaffold contract as [`write_claude_md`]: everything outside the
/// regions is written once and left alone on a later `rdc init` (unless
/// `force`), while the regions are refreshed every time so they never drift
/// from `rdc.toml`.
///
/// Title is the project root's basename (matches the user's mental
/// model of "what is this repo called"); falls back to a generic title
/// when the basename isn't valid UTF-8 or is empty.
fn write_readme(root: &Path, cfg: &ProjectConfig, force: bool) -> Result<Scaffolded> {
    let path = root.join("README.md");
    let title = root
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "Rossum configuration".to_string());

    let mut md = String::new();
    md.push_str(&format!("# {title}\n\n"));
    md.push_str(
        "Rossum.ai configuration managed by \
         [rdc](https://github.com/mrtnzlml/rdc). Each env's live state is \
         mirrored as files under `envs/<env>/` so it can be reviewed, edited, \
         and deployed like code.\n\n",
    );

    md.push_str("## Sync each environment\n\n");
    md.push_str(
        "`rdc sync <env>` reconciles the local snapshot with the remote \
         env in one pass — pulls remote edits, pushes local edits, \
         creates new objects. Run it after cloning, after pulling new \
         commits, and whenever you've edited files under `envs/<env>/`.\n\n",
    );
    md.push_str("<!-- >>> rdc:sync (generated from rdc.toml — `rdc init` refreshes it) -->\n");
    md.push_str("<!-- <<< rdc:sync -->\n\n");

    md.push_str("## Environments\n\n");
    md.push_str("<!-- >>> rdc:envs (generated from rdc.toml — `rdc init` refreshes it) -->\n");
    md.push_str("<!-- <<< rdc:envs -->\n\n");

    md.push_str("## Promote changes between environments\n\n");
    md.push_str(
        "Promotion is local-first: `rdc migrate` rewrites the target env's \
         files with zero remote calls, so the whole transform is reviewable \
         with `git diff` before a separate `rdc sync` sends any of it to a \
         tenant.\n\n",
    );
    md.push_str("<!-- >>> rdc:promote (generated from rdc.toml — `rdc init` refreshes it) -->\n");
    md.push_str("<!-- <<< rdc:promote -->\n\n");

    md.push_str("## See also\n\n");
    md.push_str(
        "- `CLAUDE.md` — editing recipes, repo layout, common commands, \
         conflict + promote workflows.\n\
         - `envs/<env>/_index.md` — auto-generated map of every object in \
         `<env>` with paths and cross-references.\n\
         - `.gitlab-ci.yml` — scheduled archive + one manual deploy button per \
         env; its header lists the CI variables to set.\n",
    );

    write_doc_with_regions(&path, &md, cfg, force)
}

/// Write the init-time scaffold files (`.gitignore`, `.gitattributes`,
/// `CLAUDE.md`, `README.md`, `.gitlab-ci.yml`, and the Python testkit) at
/// `cwd`, for a project whose env set includes `env_name`. Idempotent: each
/// underlying writer creates its file when absent and, for the three markered
/// ones, refreshes only the generated regions.
///
/// Exposed for embedders (e.g. the Rossum Local desktop app) that need
/// a Connection folder to look identical to one produced by `rdc init`
/// — including the agent guide that Claude Code reads.
///
/// The env set comes from `cwd/rdc.toml` when there is one, with `env_name`
/// added if it is missing, and is synthesized from the arguments only for a
/// genuinely new folder. This matters because the generated regions describe
/// *every* env: rendering them from the one env an embedder happens to be
/// syncing would delete the other envs' archive jobs from the pipeline and
/// shrink the guide's env table to a single row — which the next local
/// `rdc init` would put straight back.
pub fn write_scaffold_files(
    cwd: &Path,
    env_name: &str,
    api_base: &str,
    org_id: u64,
) -> Result<()> {
    write_gitignore(cwd)?;
    write_gitattributes(cwd)?;
    let cfg_path = cwd.join("rdc.toml");
    let mut cfg = if cfg_path.exists() {
        // An unreadable/undecodable rdc.toml is the local `rdc` commands'
        // problem to report; here it just means "fall back to what the caller
        // told us" rather than failing a sync that hasn't started.
        ProjectConfig::load(&cfg_path).unwrap_or_default()
    } else {
        ProjectConfig::default()
    };
    cfg.envs
        .entry(env_name.to_string())
        .or_insert_with(|| crate::config::EnvConfig {
            api_base: api_base.to_string(),
            org_id,
        });

    // These three files are parsed on every embedder call now, and a malformed
    // marker is a hard error — so a stray duplicated region in a file the user
    // hand-edited must not take down a sync before it makes its first call.
    // Degrade to leaving that one file exactly as it is.
    let log = crate::log::Log::new(crate::cli::resolve::detect_color_mode());
    let tolerate = |what: &str, outcome: Result<Scaffolded>| {
        if let Err(e) = outcome {
            log.event(
                crate::log::Action::Warn,
                &format!("{what}: {e:#}; leaving the file unchanged"),
            );
        }
    };
    tolerate("CLAUDE.md", write_claude_md(cwd, &cfg, false));
    tolerate("README.md", write_readme(cwd, &cfg, false));
    tolerate(".gitlab-ci.yml", write_gitlab_ci(cwd, &cfg, false));
    write_testkit(cwd, false)?;
    write_env_scaffolds(cwd, env_name)?;
    Ok(())
}

/// The GitLab CI pipeline `rdc init` drops into a project, embedded from the
/// repo's `templates/gitlab-ci.yml` (see [`write_gitlab_ci`]).
pub(crate) const GITLAB_CI_TEMPLATE: &str = include_str!("../../templates/gitlab-ci.yml");

/// The commented `overlay.toml` every env gets (see [`write_env_scaffolds`]),
/// embedded from `templates/overlay.toml` like the pipeline is.
///
/// Everything in it is a comment except `version = 1` — which is not
/// decoration: `Overlay::version` has no serde default, so a comments-only
/// file would fail to parse and take `rdc migrate` down with it.
pub(crate) const OVERLAY_TEMPLATE: &str = include_str!("../../templates/overlay.toml");

/// The Python test harness `rdc init` scaffolds, embedded from `templates/`
/// like the pipeline is, so the shipped copy and the repo copy cannot drift.
/// Paired as (path relative to the project root, body).
const TESTKIT_TEMPLATES: [(&str, &str); 6] = [
    ("testkit/__init__.py", include_str!("../../templates/testkit/__init__.py")),
    ("testkit/txscript_eval.py", include_str!("../../templates/testkit/txscript_eval.py")),
    (
        "testkit/test_txscript_eval.py",
        include_str!("../../templates/testkit/test_txscript_eval.py"),
    ),
    ("conftest.py", include_str!("../../templates/conftest.py")),
    ("pytest.ini", include_str!("../../templates/pytest.ini")),
    ("requirements-dev.txt", include_str!("../../templates/requirements-dev.txt")),
];

const CLAUDE_MD_TEMPLATE: &str = r#"# Agent guide

This project is managed with **rdc** (Rossum Deployment as Code). It
snapshots a Rossum.ai tenant's configuration (hooks, queues, schemas,
…) as plain files so they can be reviewed, edited, and deployed like
code.

## Where to look first

- **`envs/<env>/_index.md`** — inventory of every object in `<env>`
  with its on-disk path, human name, type-specific signals, and the
  related objects it points at (or that point at it). Start here when
  you need to find something or understand the shape of an env.
  Regenerated on every `rdc sync` — never hand-edit.
- **`rdc.toml`** — the per-env API base URL and org id. There is no project
  name; the config is just envs.

## Environments

<!-- >>> rdc:envs (generated from rdc.toml — `rdc init` refreshes it) -->
<!-- <<< rdc:envs -->

## Repo layout

```
rdc.toml                                  project + env definitions
.gitlab-ci.yml                            archive per env + one deploy draft per env;
                                          the `# >>> rdc:` regions are generated
testkit/                                  formula/hook test harness (real txscript); `pytest -q`
tests/                                    your own tests (this is where they go)
requirements-dev.txt                      pinned pytest + txscript for the CI test job
secrets/<env>.secrets.json                API tokens (gitignored)
secrets/<env>.hook-secrets.json           hook secret values (gitignored); one
                                          entry per hook slug, never copied
                                          between envs, never read back from
                                          Rossum -- this file is the only copy
envs/<env>/
  _index.md                               auto-regenerated; do not edit
  organization.json
  overlay.toml                            per-env overrides applied by
                                          `rdc migrate`; the file explains its
                                          own format
  workspaces/<ws>/
    workspace.json
    queues/<q>/
      queue.json
      schema.json
      formulas/<field_id>.py              extracted from schema content
      inbox.json
      email-templates/<slug>.json
  hooks/<slug>.json                       + sibling <slug>.py or <slug>.js for function hooks
  rules/<slug>.json                       + sibling <slug>.py for trigger_condition
  labels/<slug>.json
  engines/<engine_slug>/
    engine.json
    fields/<field_slug>.json                each engine field nests under its engine
  workflows/<workflow_slug>/
    workflow.json
    steps/<step_slug>.json                  each step nests under its workflow
  mdh/<dataset>/                          Master Data Hub (if enabled)
                                          collection.json + indexes.json,
                                          plus data.jsonl when "data": "manual"
.rdc/
  state/<env>.lock.json                   slug↔id + base hashes; never edit
  mapping.toml                            cross-env slug names, one file for
                                          every env pair; hand-editable
```

## Editing recipes

| To change… | Edit | Then run |
|---|---|---|
| Hook code (Python or Node.js) | `envs/<env>/hooks/<slug>.py` or `…/<slug>.js` | `rdc sync <env>` |
| Hook config (events, queues, name) | `envs/<env>/hooks/<slug>.json` | `rdc sync <env>` |
| Schema fields | `envs/<env>/workspaces/<ws>/queues/<q>/schema.json` | `rdc sync <env>` |
| A formula | `envs/<env>/workspaces/<ws>/queues/<q>/formulas/<field>.py` | `pytest -q`, then `rdc sync <env>` |
| Queue settings | `envs/<env>/workspaces/<ws>/queues/<q>/queue.json` | `rdc sync <env>` |
| Rule's trigger condition (Python) | `envs/<env>/rules/<slug>.py` | `rdc sync <env>` |
| Rule config (name, queues) | `envs/<env>/rules/<slug>.json` | `rdc sync <env>` |
| Label name / colour | `envs/<env>/labels/<slug>.json` | `rdc sync <env>` |
| Email template | `envs/<env>/workspaces/<ws>/queues/<q>/email-templates/<slug>.json` | `rdc sync <env>` |
| A value this env must keep across promotions | `envs/<env>/overlay.toml` | `rdc migrate <src> <env>` |
| A hook's secret values | `secrets/<env>.hook-secrets.json` | `rdc sync <env>` |

## Testing formulas and hooks

`testkit/` is a test harness that runs the **real** txscript runtime — the same
one Rossum executes — against the files in this snapshot. `pytest -q` runs it
(`pip install -r requirements-dev.txt` first), and the pipeline's `pytest` job
runs the same command; the deploy buttons depend on it.

Put your own tests under `tests/` (e.g. `tests/test_totals.py`):

```python
from testkit import evaluate_formula

FORMULAS = "envs/dev/workspaces/<ws>/queues/<q>/formulas"

def test_total_is_net_plus_tax():
    assert evaluate_formula(f"{FORMULAS}/amount_total.py",
                            amount_net="100", amount_tax="21") == 121.0
```

Field values are passed as keyword arguments named after the schema id, and the
queue's own `schema.json` gives each one its real type. A `date` field must be
given the ISO form `YYYY-MM-DD` — that is the only form the runtime parses, and
anything else reads as empty, exactly as it would in the tenant. Table columns
take `rows={"<multivalue_id>": [{...}, {...}]}` and return one value per row.
`load_hook("envs/<env>/hooks/<slug>.py")` imports a function hook so its
handlers can be called directly.

## Adding a new object

Create the JSON (and `.py` if the kind has executable code) under the
right directory. `rdc sync <env>` detects files with no lockfile entry,
POSTs them, and writes the server-assigned `id` / `url` back into the
local file. Cross-references must use URLs already known to the
lockfile (e.g. a new hook's `queues` field must point at queues that
already exist on the remote).

## Deleting an object

The snapshot is the declared state of the environment, including
absence. Remove the local file (`rm envs/<env>/labels/foo.json`) and
the next `rdc sync <env>` will detect the tombstone — the lockfile
entry remains, signalling "this object was tracked and is now gone."

Two intentional acts are required before the DELETE hits the remote:

1. Removing the file (you did this).
2. Either answering `y` to the interactive batch prompt on a TTY, or
   passing `--allow-deletes`. `--yes` does NOT bypass — destruction
   needs its own authorisation.

In non-TTY (CI) mode, `--allow-deletes` is mandatory; without it the
sync refuses with a clear list of pending tombstones.

`rdc sync <env> --dry-run` lists pending tombstones in a `deletes:`
section without sending anything.

Deletes run before creates / updates, in reverse dependency order
(`engine_fields → engines → labels → saved_views → rules →
hooks → email_templates → inboxes → queues → schemas →
workspaces`) so a queue is gone before the workspace that contained
it. The Rossum DELETE endpoint accepts 404 as success, so an object
already absent on the remote just gets its lockfile entry cleaned up.

If the remote has been modified since the last sync, an inline
resolver opens — `[k]eep delete` / `[s]kip` / `[a]bort` — so a
tombstoned delete can't silently overwrite a remote update.

## Redacted fields

To keep git diffs quiet across syncs, rdc replaces a few server-set
runtime fields with a sentinel string on disk. The key stays visible
so you (and any agent reading the snapshot) see that the field exists
in Rossum — only its value is suppressed.

| Kind | Field | Why |
|---|---|---|
| queues | `counts` | Per-status document counts. Updates whenever a document moves through review states; the real value is always live in the Rossum UI / API. |

`rdc` strips these fields from outgoing PATCH bodies automatically,
so the sentinel never reaches the server.

## Common commands

- `rdc sync <env>` — reconcile local snapshot and remote in one pass;
  pulls remote edits + pushes local edits + creates new objects;
  `--dry-run` previews changes without writing; `--allow-deletes` to
  also remove remote objects whose local files you've deleted;
  `--no-push` for read-only audit; `--no-pull` to deploy local edits
  without overwriting local files
- `rdc migrate <src> <tgt>` — copy one env's snapshot into another's,
  locally (slug remap + `rdc://` ref rewrite + target overlay; zero
  remote calls); review with `git diff`, then `rdc sync <tgt>` to push;
  `--dry-run` previews, `--only <kind>/<slug>` narrows scope
- `rdc doctor <env>` — offline check-up: reports what is on disk but not
  yet pushed and the defects that would make `rdc sync` refuse (fields
  over the API's length limit, fields missing on create, a queue bound to
  two engines), then realigns stale local slugs after a rename and prunes
  orphan base-cache entries. It writes unless you pass `--dry-run`

## Conflicts & drift

A three-way merge (local · base · remote) runs on every sync. When
both sides have diverged, `rdc sync` prompts an inline resolver:
`[k] keep local · [r] use <env> · [e] edit · [s] skip (shadow file) ·
[a] abort` (plus `[h] hunk-by-hunk` for multi-hunk bodies). In non-TTY
(CI / `--yes`) mode, conflicts park the remote copy at
`.rdc/conflicts/<env>/<same path under envs/<env>/>` and keep the local
file on disk unchanged. That tree is gitignored, and the lockfile's base
hash is held back, so the next sync raises the same conflict again.

The same drift check runs before each PATCH on the push side. The
prompt is `[k]` (force-push), `[r]` (adopt remote), `[s]` (skip),
`[a]` (abort).

## Promoting changes between environments

<!-- >>> rdc:promote (generated from rdc.toml — `rdc init` refreshes it) -->
<!-- <<< rdc:promote -->

## What NOT to edit

- `_index.md` (auto-regenerated by every sync)
- `.rdc/state/<env>.lock.json` (slug↔id and base hashes; rdc owns this)
- `.rdc/conflicts/<env>/` (the remote side of an unresolved conflict;
  either consume the changes or delete the file)
- `secrets/<env>.secrets.json` (rdc's token cache). Its neighbour
  `secrets/<env>.hook-secrets.json` IS yours to edit — that is where hook
  secret values go, and nothing else has a copy of them

## Known limitations

- **One schema per queue, one inbox per queue.** The Rossum API
  technically allows a single schema or inbox to be referenced by many
  queues (`schema.queues` / `inbox.queues` are arrays). rdc's on-disk
  layout assumes each queue owns its own schema and inbox — the files
  live at `workspaces/<ws>/queues/<q>/schema.json` and `inbox.json`.
  If a tenant shares a schema or inbox across queues, rdc will write
  duplicate copies (one per consuming queue) and the lockfile will
  carry an entry per queue slug pointing at the same remote id. Push
  to that shared resource works (id is the truth), but cross-env
  promotion via `rdc migrate` may surface confusing diffs.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedder entry point (desktop Connection folders) must produce the
    /// same scaffold `rdc init` does — a Connection folder is meant to be
    /// indistinguishable from a hand-initialised project.
    #[test]
    fn write_scaffold_files_writes_every_scaffold_file() {
        let dir = tempfile::TempDir::new().unwrap();
        write_scaffold_files(dir.path(), "main", "https://example.rossum.app/api/v1", 1).unwrap();
        for name in [
            ".gitignore",
            ".gitattributes",
            "CLAUDE.md",
            "README.md",
            ".gitlab-ci.yml",
            "testkit/__init__.py",
            "testkit/txscript_eval.py",
            "testkit/test_txscript_eval.py",
            "conftest.py",
            "pytest.ini",
            "requirements-dev.txt",
            "envs/main/overlay.toml",
            "secrets/main.hook-secrets.json",
        ] {
            assert!(dir.path().join(name).exists(), "{name} should be written");
        }
        let mut cfg = ProjectConfig::default();
        cfg.envs.insert(
            "main".to_string(),
            EnvConfig {
                api_base: "https://example.rossum.app/api/v1".to_string(),
                org_id: 1,
            },
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitlab-ci.yml")).unwrap(),
            crate::cli::gitlab_ci::generate(GITLAB_CI_TEMPLATE, &cfg.envs).unwrap()
        );
    }

    /// A hand-emptied `rdc.toml` (no envs at all) must not reach the splicer:
    /// `render_archive_envs` would emit an empty `parallel:matrix`, which
    /// isn't valid YAML. Falling back to the verbatim template keeps the
    /// committed example (`dev`/`prod`/`test`) intact instead.
    #[test]
    fn write_gitlab_ci_with_no_envs_writes_the_template_verbatim() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = ProjectConfig::default();
        assert!(cfg.envs.is_empty());

        write_gitlab_ci(dir.path(), &cfg, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitlab-ci.yml")).unwrap(),
            GITLAB_CI_TEMPLATE
        );
    }

    /// Embedders call this on every sync, so it must stay non-destructive.
    #[test]
    fn write_scaffold_files_never_clobbers_existing_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitlab-ci.yml"), "mine\n").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "mine\n").unwrap();

        write_scaffold_files(dir.path(), "main", "https://example.rossum.app/api/v1", 1).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitlab-ci.yml")).unwrap(),
            "mine\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap(),
            "mine\n"
        );
    }

    /// A two-env project as `write_scaffold_files` leaves it, plus the config
    /// that describes it. The embedder is handed ONE env; everything generated
    /// here describes both.
    fn two_env_project() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = ProjectConfig::default();
        for (name, org_id) in [("dev", 1u64), ("test", 2)] {
            cfg.envs.insert(
                name.to_string(),
                EnvConfig {
                    api_base: "https://example.rossum.app/api/v1".to_string(),
                    org_id,
                },
            );
        }
        cfg.save(&dir.path().join("rdc.toml")).unwrap();
        write_scaffold_files(dir.path(), "dev", "https://example.rossum.app/api/v1", 1).unwrap();
        dir
    }

    /// The markered files are spliced on every embedder call, so the env set
    /// they are spliced with must be the project's real one. Rendering them
    /// from the single env an embedder happens to be syncing would delete the
    /// other envs' archive jobs -- silently stopping their archive -- and shrink
    /// the guide's env table to one row.
    #[test]
    fn write_scaffold_files_keeps_the_other_envs_regions() {
        let dir = two_env_project();
        let ci_path = dir.path().join(".gitlab-ci.yml");

        // generated from rdc.toml, not from the one env we were handed
        let ci = std::fs::read_to_string(&ci_path).unwrap();
        assert!(ci.contains("- RDC_ENV: \"dev\""), "{ci}");
        assert!(ci.contains("- RDC_ENV: \"test\""), "{ci}");
        assert!(std::fs::read_to_string(dir.path().join("CLAUDE.md")).unwrap().contains("| `test` |"));

        // a hand edit outside the regions, then another embedder sync
        let edited = format!("{ci}\nmy-own-job:\n  script:\n    - echo mine\n");
        std::fs::write(&ci_path, &edited).unwrap();
        write_scaffold_files(dir.path(), "dev", "https://example.rossum.app/api/v1", 1).unwrap();

        assert_eq!(
            std::fs::read_to_string(&ci_path).unwrap(),
            edited,
            "a second call must change nothing at all"
        );
        assert!(std::fs::read_to_string(dir.path().join("README.md"))
            .unwrap()
            .contains("rdc sync test"));
    }

    /// A malformed marker is a hard error in the splicer. On this path that
    /// error must not fail the caller before it has made a single network call:
    /// the file is left alone and the run continues.
    #[test]
    fn write_scaffold_files_tolerates_a_malformed_marker() {
        let dir = two_env_project();
        let ci_path = dir.path().join(".gitlab-ci.yml");
        // the classic: a region pasted twice
        let broken = format!(
            "{}\n# >>> rdc:deploy-jobs\n# <<< rdc:deploy-jobs\n",
            std::fs::read_to_string(&ci_path).unwrap()
        );
        std::fs::write(&ci_path, &broken).unwrap();

        write_scaffold_files(dir.path(), "dev", "https://example.rossum.app/api/v1", 1)
            .expect("a user's broken marker must not fail the embedder");

        assert_eq!(std::fs::read_to_string(&ci_path).unwrap(), broken);
    }

    /// Create → skip → (force) rewrite only when the bytes actually differ, so
    /// `rdc init --force` on an untouched project churns no mtimes.
    #[test]
    fn write_template_file_force_semantics() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("scaffold.txt");

        assert_eq!(
            write_template_file(&p, "body\n", false).unwrap(),
            Scaffolded::Created
        );
        assert_eq!(
            write_template_file(&p, "body\n", false).unwrap(),
            Scaffolded::Unchanged
        );
        assert_eq!(
            write_template_file(&p, "body\n", true).unwrap(),
            Scaffolded::Unchanged
        );

        std::fs::write(&p, "drifted\n").unwrap();
        assert_eq!(
            write_template_file(&p, "body\n", false).unwrap(),
            Scaffolded::Unchanged,
            "without --force a drifted file is left alone"
        );
        assert_eq!(
            write_template_file(&p, "body\n", true).unwrap(),
            Scaffolded::Rewritten
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "body\n");
    }

    /// A hand-edited file that isn't valid UTF-8 must not fail the run: the
    /// force comparison reads bytes, not a `String`.
    #[test]
    fn write_template_file_force_handles_non_utf8() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("scaffold.txt");
        std::fs::write(&p, [0xff, 0xfe, 0x00]).unwrap();

        assert_eq!(
            write_template_file(&p, "body\n", true).unwrap(),
            Scaffolded::Rewritten
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "body\n");
    }

    /// `version = 1` in the template is load-bearing, not decoration:
    /// `Overlay::version` has no serde default, so a comments-only file fails
    /// to parse and would take `rdc migrate` down on every project that
    /// scaffolded one. The other half is that EVERY example stays commented —
    /// an uncommented one names a slug no env has, which migrate rejects.
    #[test]
    fn embedded_overlay_template_loads_as_an_empty_overlay() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("overlay.toml");
        std::fs::write(&p, OVERLAY_TEMPLATE).unwrap();

        let ov = crate::overlay::Overlay::load(&p)
            .expect("the scaffolded overlay must parse")
            .expect("the file is present");
        assert_eq!(ov.version, crate::overlay::OVERLAY_VERSION);
        assert!(
            ov.hooks.is_empty()
                && ov.queues.is_empty()
                && ov.inboxes.is_empty()
                && ov.saved_views.is_empty()
                && ov.organization.is_empty(),
            "every example must still be commented out: {ov:?}"
        );
    }

    /// Once written, both files hold the user's own data — hand-written
    /// overrides, and secret values that exist in no other copy. So unlike
    /// every other scaffold they have no `--force` path back to the template.
    #[test]
    fn write_env_scaffolds_creates_both_then_never_touches_them() {
        let dir = tempfile::TempDir::new().unwrap();
        let first = write_env_scaffolds(dir.path(), "dev").unwrap();
        assert!(
            first.iter().all(|(_, o)| *o == Scaffolded::Created),
            "{first:?}"
        );

        let overlay = dir.path().join("envs/dev/overlay.toml");
        let secrets = dir.path().join("secrets/dev.hook-secrets.json");
        std::fs::write(&overlay, "version = 1

[queues.invoices]
name = \"mine\"
").unwrap();
        std::fs::write(&secrets, r#"{"hooks":{"h":{"k":"kept"}}}"#).unwrap();

        let again = write_env_scaffolds(dir.path(), "dev").unwrap();
        assert!(
            again.iter().all(|(_, o)| *o == Scaffolded::Unchanged),
            "{again:?}"
        );
        assert!(std::fs::read_to_string(&overlay).unwrap().contains("mine"));
        assert!(std::fs::read_to_string(&secrets).unwrap().contains("kept"));
    }

    /// Smoke-check the `include_str!` target: the constant is the real pipeline,
    /// not an empty or hand-copied stand-in.
    #[test]
    fn embedded_gitlab_ci_template_is_the_repo_pipeline() {
        assert!(GITLAB_CI_TEMPLATE.starts_with("# GitLab CI for a Rossum project managed with rdc"));
        assert!(GITLAB_CI_TEMPLATE.contains("rdc sync \"$RDC_ENV\" --no-push --yes --conflict use-remote"));
        assert!(GITLAB_CI_TEMPLATE.contains("rdc migrate \"$RDC_SRC\" \"$RDC_ENV\" --mirror --yes"));
    }
}
